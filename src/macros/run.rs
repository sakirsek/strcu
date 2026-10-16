//! Running macros, one at a time: each step goes to the worker thread on its own, so the live view keeps
//! coming between steps. Also starts scheduled macros and records new ones from what is done in the panel.
//!
//! A run stops when someone uses the mouse or keyboard at the computer: input arriving later than the macro's
//! own last input is someone else's (Windows reports when the last input came, not whose it was).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::schedule::Minute;
use super::*;
use crate::i18n::Arg;
use crate::server::{ActionLog, SHUTDOWN_SECS, local_time};
use crate::sys::{Rect, apps, input, power, screen, window};
use crate::worker::{Ctx, Worker};

/// How long a step looks for its window or element (the app may still be opening or drawing)
const FIND_FOR: Duration = Duration::from_secs(5);
/// Pause between steps, as a person would leave
const BETWEEN: Duration = Duration::from_millis(350);
/// Input this long after a step's own still counts as the step's
const OWN_INPUT_MS: u32 = 400;
/// A scheduled macro does not start if the computer was used this recently
const IN_USE_MS: u32 = 5000;
/// "Run a macro" inside "run a macro", at most
const MAX_DEPTH: usize = 3;
/// Steps one run takes at most, repeats included: a guard against a loop that never ends
const MAX_RUN_STEPS: u64 = 100_000;

#[derive(Clone, Serialize)]
struct Running {
    id: String,
    name: String,
    /// The step running now, from 1
    step: usize,
    total: usize,
}

struct Recording {
    /// Tells this recording from a later one
    seq: u64,
    steps: Vec<Step>,
}

enum Halt {
    Failed(Msg),
    /// By the stop button, the panel or someone at the computer
    Stopped(Msg),
}

enum Flow {
    Next,
    /// Locked or shutting down: nothing can follow
    End,
}

/// Steps running (boxed: "run a macro" runs steps inside steps); an error comes with its step's number.
type Steps<'a> = Pin<Box<dyn Future<Output = Result<Flow, (usize, Halt)>> + Send + 'a>>;

pub struct Engine {
    worker: Worker,
    log: Arc<ActionLog>,
    shutdown_at: Arc<Mutex<Option<Instant>>>,
    list: Mutex<Vec<Macro>>,
    running: Mutex<Option<Running>>,
    /// A single step is being tried from the editor
    trying: AtomicBool,
    rec: Mutex<Option<Recording>>,
    rec_seq: AtomicU64,
    stop: AtomicBool,
    why: Mutex<Option<Msg>>,
    wake: Notify,
    /// Input after this tick is not the macro's
    quiet: AtomicU32,
    taken: AtomicU64,
}

fn as_value(m: &Msg) -> Value {
    serde_json::to_value(m).unwrap_or_default()
}

fn failed(e: anyhow::Error) -> Halt {
    Halt::Failed(i18n::from_error(&e))
}

impl Engine {
    pub fn new(worker: Worker, log: Arc<ActionLog>, shutdown_at: Arc<Mutex<Option<Instant>>>) -> Arc<Engine> {
        Arc::new(Engine {
            worker,
            log,
            shutdown_at,
            list: Mutex::new(read_all()),
            running: Mutex::new(None),
            trying: AtomicBool::new(false),
            rec: Mutex::new(None),
            rec_seq: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            why: Mutex::new(None),
            wake: Notify::new(),
            quiet: AtomicU32::new(0),
            taken: AtomicU64::new(0),
        })
    }

    // ---------- the list ----------

    fn get(&self, id: &str) -> Option<Macro> {
        self.list.lock().unwrap().iter().find(|m| m.id == id).cloned()
    }

    /// Every macro with its next scheduled start and whether a failure screenshot is kept, plus what runs now.
    pub fn list_json(&self) -> Value {
        let now = Minute::now();
        let items: Vec<Value> = self
            .list
            .lock()
            .unwrap()
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap_or_default();
                v["next"] = json!(m.schedule.as_ref().and_then(|s| s.next(&now)).map(|n| n.key()));
                v["shot"] = json!(shot_path(&m.id).exists());
                v
            })
            .collect();
        json!({ "macros": items, "now": now.key(), "state": self.state() })
    }

    /// What runs or is being recorded, for the panel's strip.
    pub fn state(&self) -> Value {
        let running = self.running.lock().unwrap().clone();
        let recording = self.rec.lock().unwrap().as_ref().map(|r| r.steps.len());
        json!({ "running": running, "recording": recording })
    }

    pub fn save(&self, d: Draft) -> Result<Macro, Msg> {
        let mut list = self.list.lock().unwrap();
        let mut m = match &d.id {
            Some(id) => list.iter().find(|m| &m.id == id).cloned().ok_or_else(|| Msg::new("err.mac_gone"))?,
            None if list.len() >= MAX_MACROS => return Err(Msg::new("err.mac_too_many_macros").with("max", MAX_MACROS)),
            None => Macro { id: new_id(), name: String::new(), steps: Vec::new(), schedule: None, runs: Vec::new() },
        };
        m.name = d.name.trim().to_string();
        m.steps = d.steps;
        m.schedule = d.schedule;
        if let Some(s) = m.schedule.as_mut() {
            s.normalize()?;
        }
        check(&m, &list)?;
        match list.iter_mut().find(|x| x.id == m.id) {
            Some(x) => *x = m.clone(),
            None => list.push(m.clone()),
        }
        write_all(&list).map_err(|e| i18n::from_error(&e))?;
        Ok(m)
    }

    /// Returns the deleted macro's name.
    pub fn delete(&self, id: &str) -> Result<String, Msg> {
        if self.running.lock().unwrap().as_ref().is_some_and(|r| r.id == id) {
            return Err(Msg::new("err.mac_busy"));
        }
        let mut list = self.list.lock().unwrap();
        let i = list.iter().position(|m| m.id == id).ok_or_else(|| Msg::new("err.mac_gone"))?;
        if let Some(user) = list.iter().find(|m| m.steps.iter().any(|s| matches!(&s.act, Act::RunMacro { id: x } if x == id))) {
            return Err(Msg::new("err.mac_used").with("name", &user.name));
        }
        let m = list.remove(i);
        write_all(&list).map_err(|e| i18n::from_error(&e))?;
        let _ = std::fs::remove_file(shot_path(id));
        Ok(m.name)
    }

    /// The screenshot from the macro's last failure.
    pub fn shot(&self, id: &str) -> Option<Vec<u8>> {
        self.get(id)?;
        std::fs::read(shot_path(id)).ok()
    }

    fn add_run(&self, id: &str, r: Run) {
        let mut list = self.list.lock().unwrap();
        if let Some(m) = list.iter_mut().find(|m| m.id == id) {
            m.runs.insert(0, r);
            m.runs.truncate(KEEP_RUNS);
        }
        let _ = write_all(&list);
    }

    // ---------- starting and stopping ----------

    /// Starts macros at their minutes. Only minutes that begin while StrCu runs count: one missed while the
    /// computer was off or asleep is not caught up.
    pub fn start_scheduler(self: &Arc<Self>) {
        let me = self.clone();
        tokio::spawn(async move {
            let mut last = Minute::now();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = Minute::now();
                if now != last {
                    last = now;
                    me.on_minute(&now);
                }
            }
        });
    }

    fn on_minute(self: &Arc<Self>, now: &Minute) {
        let due: Vec<Macro> = self
            .list
            .lock()
            .unwrap()
            .iter()
            .filter(|m| !m.steps.is_empty() && m.schedule.as_ref().is_some_and(|s| s.due(now)))
            .cloned()
            .collect();
        for m in due {
            match self.claim(&m, true) {
                Ok(()) => self.spawn_run(m, "schedule"),
                Err(why) => {
                    self.log.add(Msg::new("ev.mac_skipped").with("name", &m.name).with("reason", why.clone()), false);
                    let run = Run {
                        at: local_time(),
                        how: "schedule".into(),
                        result: "skipped".into(),
                        step: None,
                        reason: Some(as_value(&why)),
                        secs: 0,
                    };
                    self.add_run(&m.id, run);
                }
            }
        }
    }

    /// Marks the macro as running, or says why it cannot run now.
    fn claim(&self, m: &Macro, scheduled: bool) -> Result<(), Msg> {
        let mut run = self.running.lock().unwrap();
        if run.is_some() || self.trying.load(SeqCst) {
            return Err(Msg::new("err.mac_busy"));
        }
        if self.rec.lock().unwrap().is_some() {
            return Err(Msg::new("err.mac_recording"));
        }
        if power::lock_state() == power::LockState::Locked {
            return Err(Msg::new("err.mac_locked"));
        }
        if scheduled && input::idle_ms() < IN_USE_MS {
            return Err(Msg::new("mac.why_in_use"));
        }
        *run = Some(Running { id: m.id.clone(), name: m.name.clone(), step: 1, total: m.steps.len() });
        Ok(())
    }

    /// Runs a macro now (the panel's ▶).
    pub fn run(self: &Arc<Self>, id: &str) -> Result<(), Msg> {
        let m = self.get(id).ok_or_else(|| Msg::new("err.mac_gone"))?;
        if m.steps.is_empty() {
            return Err(Msg::new("err.mac_empty"));
        }
        self.claim(&m, false)?;
        self.spawn_run(m, "manual");
        Ok(())
    }

    fn reset(&self) {
        self.stop.store(false, SeqCst);
        *self.why.lock().unwrap() = None;
        self.quiet.store(input::tick(), SeqCst);
        self.taken.store(0, SeqCst);
    }

    fn spawn_run(self: &Arc<Self>, m: Macro, how: &'static str) {
        self.reset();
        tokio::spawn(self.clone().run_task(m, how));
    }

    /// The stop button. False if nothing was running.
    pub fn stop(&self) -> bool {
        let running = self.running.lock().unwrap().is_some();
        if running {
            self.halt(Msg::new("mac.why_button"));
        }
        running
    }

    /// The panel is about to click or type itself: the macro must not carry on under it.
    pub fn interrupt(&self) {
        if self.running.lock().unwrap().is_some() {
            self.halt(Msg::new("mac.why_panel"));
        }
    }

    fn halt(&self, why: Msg) {
        self.why.lock().unwrap().get_or_insert(why);
        self.stop.store(true, SeqCst);
        self.wake.notify_waiters();
    }

    async fn run_task(self: Arc<Self>, m: Macro, how: &'static str) {
        let (at, started) = (local_time(), Instant::now());
        let begun = if how == "schedule" { "ev.mac_scheduled" } else { "ev.mac_started" };
        self.log.add(Msg::new(begun).with("name", &m.name), true);
        let (result, step, reason) = match self.run_steps(&m.steps, 0, true).await {
            Ok(_) => {
                self.log.add(Msg::new("ev.mac_done").with("name", &m.name), true);
                ("ok", None, None)
            }
            Err((n, Halt::Stopped(why))) => {
                self.log.add(Msg::new("ev.mac_stopped").with("name", &m.name).with("reason", why.clone()), false);
                ("stopped", Some(n), Some(why))
            }
            Err((n, Halt::Failed(why))) => {
                self.save_shot(&m.id).await;
                self.log.add(Msg::new("ev.mac_failed").with("name", &m.name).with("n", n).with("reason", why.clone()), false);
                ("failed", Some(n), Some(why))
            }
        };
        let secs = started.elapsed().as_secs();
        let reason = reason.as_ref().map(as_value);
        self.add_run(&m.id, Run { at, how: how.into(), result: result.into(), step, reason, secs });
        *self.running.lock().unwrap() = None;
    }

    /// Runs steps in order; an error comes back with the step's number (from 1).
    fn run_steps<'a>(&'a self, steps: &'a [Step], depth: usize, top: bool) -> Steps<'a> {
        Box::pin(async move {
            // How many times each Repeat step has sent the run back
            let mut rounds = vec![0u32; steps.len()];
            let mut i = 0;
            while i < steps.len() {
                let n = i + 1;
                if top && let Some(r) = self.running.lock().unwrap().as_mut() {
                    r.step = n;
                }
                self.check().map_err(|h| (n, h))?;
                if self.taken.fetch_add(1, SeqCst) >= MAX_RUN_STEPS {
                    return Err((n, Halt::Failed(Msg::new("err.mac_runaway"))));
                }
                let step = &steps[i];
                let r = match &step.act {
                    Act::Repeat { back, times } => {
                        if rounds[i] + 1 < *times {
                            rounds[i] += 1;
                            i = i.saturating_sub(*back);
                            continue;
                        }
                        rounds[i] = 0;
                        Ok(Flow::Next)
                    }
                    Act::RunMacro { id } => match self.get(id) {
                        None => Err(Halt::Failed(Msg::new("err.mac_gone"))),
                        Some(_) if depth + 1 >= MAX_DEPTH => Err(Halt::Failed(Msg::new("err.mac_too_deep"))),
                        Some(inner) => match self.run_steps(&inner.steps, depth + 1, false).await {
                            Err((k, Halt::Failed(m))) => {
                                Err(Halt::Failed(Msg::new("err.mac_inner").with("name", &inner.name).with("n", k).with("msg", m)))
                            }
                            other => other.map_err(|(_, h)| h),
                        },
                    },
                    act => self.exec(act).await,
                };
                match r {
                    Ok(Flow::End) => return Ok(Flow::End),
                    Ok(Flow::Next) => {}
                    Err(Halt::Failed(_)) if step.go_on => {}
                    Err(h) => return Err((n, h)),
                }
                i += 1;
                if i < steps.len() {
                    self.pause(BETWEEN).await.map_err(|h| (n, h))?;
                }
            }
            Ok(Flow::Next)
        })
    }

    /// Before every step and while waiting: stopped, someone at the computer, or locked?
    fn check(&self) -> Result<(), Halt> {
        if self.stop.load(SeqCst) {
            let why = self.why.lock().unwrap().clone().unwrap_or_else(|| Msg::new("mac.why_button"));
            return Err(Halt::Stopped(why));
        }
        if input::last_input_tick().wrapping_sub(self.quiet.load(SeqCst)) as i32 > 0 {
            let why = Msg::new("mac.why_user");
            self.halt(why.clone());
            return Err(Halt::Stopped(why));
        }
        if power::lock_state() == power::LockState::Locked {
            return Err(Halt::Failed(Msg::new("err.mac_locked")));
        }
        Ok(())
    }

    /// The input of a step just sent is the macro's own.
    fn settle(&self) {
        self.quiet.store(input::tick().wrapping_add(OWN_INPUT_MS), SeqCst);
    }

    async fn pause(&self, d: Duration) -> Result<(), Halt> {
        let end = Instant::now() + d;
        loop {
            self.check()?;
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            tokio::select! {
                _ = tokio::time::sleep(left.min(Duration::from_millis(250))) => {}
                _ = self.wake.notified() => {}
            }
        }
    }

    async fn exec(&self, act: &Act) -> Result<Flow, Halt> {
        let flow = match act {
            Act::Click { target, button, count } => {
                let (button, count) = (*button, *count);
                self.at(vec![target.clone()], move |ctx, p| {
                    input::click(p[0].0, p[0].1, button, count)?;
                    ctx.note_click(p[0].0, p[0].1);
                    Ok(())
                })
                .await?;
                Flow::Next
            }
            Act::Scroll { target, notches } => {
                let n = *notches;
                self.at(vec![target.clone()], move |_, p| input::scroll(p[0].0, p[0].1, n)).await?;
                Flow::Next
            }
            Act::Drag { from, to } => {
                self.at(vec![from.clone(), to.clone()], |_, p| input::drag(p[0], p[1])).await?;
                Flow::Next
            }
            Act::Type { text, enter } => {
                let (text, enter) = (fill_in(text, &Minute::now()), *enter);
                let r = self
                    .worker
                    .run(move |_| {
                        input::type_text(&text)?;
                        if enter {
                            input::hotkey("enter")?;
                        }
                        Ok(())
                    })
                    .await;
                self.settle();
                r.map_err(failed)?;
                Flow::Next
            }
            Act::Key { combo, times } => {
                for k in 0..*times {
                    if k > 0 {
                        self.pause(Duration::from_millis(80)).await?;
                    }
                    let c = combo.clone();
                    let r = self.worker.run(move |_| input::hotkey(&c)).await;
                    self.settle();
                    r.map_err(failed)?;
                }
                Flow::Next
            }
            Act::Launch { id, .. } => {
                let id = id.clone();
                tokio::task::spawn_blocking(move || apps::launch(&id)).await.map_err(|e| failed(e.into()))?.map_err(failed)?;
                Flow::Next
            }
            Act::Open { path } => {
                let p = path.clone();
                tokio::task::spawn_blocking(move || apps::open(&p)).await.map_err(|e| failed(e.into()))?.map_err(failed)?;
                Flow::Next
            }
            Act::Window { app, op } => {
                self.window(app, *op).await?;
                Flow::Next
            }
            Act::Wait { secs } => {
                self.pause(Duration::from_secs_f64(*secs)).await?;
                Flow::Next
            }
            Act::WaitWindow { app, timeout } => {
                self.wait_for(*timeout, Msg::new("err.mac_wait_window").with("app", app_arg(app)), {
                    let app = app.clone();
                    move |_| Ok(find_window(&app).is_some())
                })
                .await?;
                Flow::Next
            }
            Act::WaitElement { target, timeout } => {
                let t = Target { by_name: true, ..target.clone() };
                self.wait_for(*timeout, Msg::new("err.mac_wait_element").with("name", &t.name), move |ctx| {
                    Ok(matches!(locate(ctx, &t, false)?, Found::At(..)))
                })
                .await?;
                Flow::Next
            }
            Act::Lock => {
                self.worker.run(|_| power::lock()).await.map_err(failed)?;
                self.log.add(Msg::new("ev.locked"), true);
                Flow::End
            }
            Act::Shutdown => {
                self.worker.run(|_| power::shutdown(SHUTDOWN_SECS)).await.map_err(failed)?;
                *self.shutdown_at.lock().unwrap() = Some(Instant::now() + Duration::from_secs(SHUTDOWN_SECS.into()));
                self.log.add(Msg::new("ev.shutdown").with("s", SHUTDOWN_SECS), true);
                Flow::End
            }
            // Run by `run_steps`
            Act::Repeat { .. } | Act::RunMacro { .. } => Flow::Next,
        };
        Ok(flow)
    }

    /// Finds the targets (the first one's window comes to the front) and acts on their points. A window or an
    /// element not there yet is looked for again for a few seconds.
    async fn at<F>(&self, targets: Vec<Target>, act: F) -> Result<(), Halt>
    where
        F: Fn(&mut Ctx, &[(i32, i32)]) -> anyhow::Result<()> + Send + Clone + 'static,
    {
        let deadline = Instant::now() + FIND_FOR;
        loop {
            let (ts, f) = (targets.clone(), act.clone());
            let r = self
                .worker
                .run(move |ctx| {
                    let mut points = Vec::with_capacity(ts.len());
                    for (k, t) in ts.iter().enumerate() {
                        match locate(ctx, t, k == 0)? {
                            Found::At(x, y) => points.push((x, y)),
                            Found::Missing(m) => return Ok(Some(m)),
                        }
                    }
                    f(ctx, &points)?;
                    Ok(None)
                })
                .await;
            // Bringing the window to the front presses Alt
            self.settle();
            match r.map_err(failed)? {
                None => return Ok(()),
                Some(m) if Instant::now() >= deadline => return Err(Halt::Failed(m)),
                Some(_) => self.pause(Duration::from_millis(500)).await?,
            }
        }
    }

    async fn window(&self, app: &AppRef, op: WinOp) -> Result<(), Halt> {
        let deadline = Instant::now() + FIND_FOR;
        loop {
            let a = app.clone();
            let r = self
                .worker
                .run(move |_| {
                    let Some(w) = find_window(&a) else { return Ok(false) };
                    match op {
                        WinOp::Front => window::focus(w.hwnd)?,
                        WinOp::Maximize => {
                            window::focus(w.hwnd)?;
                            window::maximize(w.hwnd);
                        }
                        WinOp::Minimize => window::minimize(w.hwnd),
                        WinOp::Close => window::close(w.hwnd)?,
                    }
                    Ok(true)
                })
                .await;
            self.settle();
            if r.map_err(failed)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Halt::Failed(Msg::new("err.mac_no_window").with("app", app_arg(app))));
            }
            self.pause(Duration::from_millis(500)).await?;
        }
    }

    /// Asks `found` twice a second until it says yes; `late` is the error after `timeout` seconds.
    async fn wait_for<F>(&self, timeout: u32, late: Msg, found: F) -> Result<(), Halt>
    where
        F: Fn(&mut Ctx) -> anyhow::Result<bool> + Send + Clone + 'static,
    {
        let deadline = Instant::now() + Duration::from_secs(timeout.into());
        loop {
            if self.worker.run(found.clone()).await.map_err(failed)? {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Halt::Failed(late.with("s", timeout)));
            }
            self.pause(Duration::from_millis(500)).await?;
        }
    }

    async fn save_shot(&self, id: &str) {
        let path = shot_path(id);
        let jpg = self
            .worker
            .run(|ctx| {
                let img = ctx.shot(screen::virtual_screen())?;
                screen::encode_jpeg(&screen::downscale(&img, 1600), 75)
            })
            .await;
        if let (Ok(b), Some(dir)) = (jpg, path.parent()) {
            let _ = std::fs::create_dir_all(dir);
            let _ = std::fs::write(&path, b);
        }
    }

    /// Runs one step from the editor ("try"), outside any macro.
    pub async fn try_step(&self, step: Step) -> Result<(), Msg> {
        if matches!(step.act, Act::Repeat { .. } | Act::RunMacro { .. } | Act::Lock | Act::Shutdown) {
            return Err(Msg::new("err.mac_no_try"));
        }
        check_step(0, &step.act, true, &[], "")?;
        {
            let run = self.running.lock().unwrap();
            if run.is_some() || self.trying.swap(true, SeqCst) {
                return Err(Msg::new("err.mac_busy"));
            }
        }
        self.reset();
        let r = self.exec(&step.act).await;
        self.trying.store(false, SeqCst);
        r.map(|_| ()).map_err(|h| match h {
            Halt::Failed(m) | Halt::Stopped(m) => m,
        })
    }

    // ---------- recording ----------

    pub fn rec_start(&self) -> Result<(), Msg> {
        let run = self.running.lock().unwrap();
        if run.is_some() {
            return Err(Msg::new("err.mac_busy"));
        }
        let mut rec = self.rec.lock().unwrap();
        if rec.is_none() {
            *rec = Some(Recording { seq: self.rec_seq.fetch_add(1, SeqCst) + 1, steps: Vec::new() });
        }
        Ok(())
    }

    /// Ends the recording and hands over its steps.
    pub fn rec_finish(&self) -> Vec<Step> {
        self.rec.lock().unwrap().take().map(|r| r.steps).unwrap_or_default()
    }

    pub fn recording(&self) -> bool {
        self.rec.lock().unwrap().is_some()
    }

    /// Adds what the panel just did. The same key pressed again becomes one step with a count.
    pub fn record(&self, act: Act) {
        if let Some(r) = self.rec.lock().unwrap().as_mut() {
            add_step(&mut r.steps, act);
        }
    }

    /// Live typing from the panel (each key sent as it is pressed): letters join the text typed just before
    /// and corrections are taken out of it, so a typed sentence is one step however it was typed.
    pub fn record_live(&self, back: u32, text: &str, enter: bool) {
        if let Some(r) = self.rec.lock().unwrap().as_mut() {
            add_live(&mut r.steps, back, text, enter);
        }
    }

    /// An app was opened while recording: if a new window shows up within 20 seconds, "wait for this window"
    /// goes right after the step, so the macro waits for the app as the recording did. `before` holds the
    /// windows that were open already.
    pub fn record_launch(self: &Arc<Self>, id: String, name: String, before: Vec<isize>) {
        let (seq, at) = {
            let mut rec = self.rec.lock().unwrap();
            let Some(r) = rec.as_mut() else { return };
            if r.steps.len() >= MAX_STEPS {
                return;
            }
            r.steps.push(Step { act: Act::Launch { id, name }, go_on: false });
            (r.seq, r.steps.len())
        };
        let me = self.clone();
        tokio::spawn(async move {
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(500)).await;
                if !me.recording_now(seq) {
                    return;
                }
                let before = before.clone();
                let new = me.worker.run(move |_| Ok(window::list().into_iter().find(|w| !w.own && !before.contains(&w.hwnd)))).await;
                match new {
                    Ok(Some(w)) => return me.insert_wait(seq, at, &w),
                    Ok(None) => {}
                    Err(_) => return,
                }
            }
        });
    }

    fn recording_now(&self, seq: u64) -> bool {
        self.rec.lock().unwrap().as_ref().is_some_and(|r| r.seq == seq)
    }

    fn insert_wait(&self, seq: u64, at: usize, w: &window::WindowInfo) {
        let mut rec = self.rec.lock().unwrap();
        if let Some(r) = rec.as_mut().filter(|r| r.seq == seq && r.steps.len() < MAX_STEPS) {
            let step = Step { act: Act::WaitWindow { app: AppRef::of(w), timeout: 30 }, go_on: false };
            r.steps.insert(at.min(r.steps.len()), step);
        }
    }
}

impl AppRef {
    pub fn of(w: &window::WindowInfo) -> AppRef {
        if w.class == "Shell_TrayWnd" {
            return AppRef { process: w.process.clone(), name: String::new(), title: String::new(), taskbar: true };
        }
        let name = if w.app.is_empty() { w.title.clone() } else { w.app.clone() };
        AppRef { process: w.process.clone(), name, title: w.title.clone(), taskbar: false }
    }
}

/// The app's name in a message ("the taskbar" in the reader's language).
fn app_arg(app: &AppRef) -> Arg {
    if app.taskbar { Arg::Msg(Msg::new("mac.taskbar")) } else { Arg::Text(app.name.clone()) }
}

/// The app's window: the one with the title it had, else the one in front, else the topmost.
fn find_window(app: &AppRef) -> Option<window::WindowInfo> {
    if app.taskbar {
        return window::taskbar();
    }
    let store = window::is_store_frame(&app.process);
    let mine: Vec<_> = window::list()
        .into_iter()
        .filter(|w| {
            !w.own
                && if store {
                    w.title.eq_ignore_ascii_case(&app.title)
                } else {
                    !app.process.is_empty() && w.process.eq_ignore_ascii_case(&app.process)
                }
        })
        .collect();
    let pick = mine.iter().position(|w| w.title == app.title).or_else(|| mine.iter().position(|w| w.foreground)).unwrap_or(0);
    mine.into_iter().nth(pick)
}

enum Found {
    At(i32, i32),
    Missing(Msg),
}

/// Where a target is now. With `focus` its window comes to the front first, as a person would bring it.
fn locate(ctx: &mut Ctx, t: &Target, focus: bool) -> anyhow::Result<Found> {
    let Some(app) = &t.app else { return Ok(Found::At(t.x, t.y)) };
    let Some(mut w) = find_window(app) else {
        return Ok(Found::Missing(Msg::new("err.mac_no_window").with("app", app_arg(app))));
    };
    if focus && !app.taskbar && !w.foreground {
        window::focus(w.hwnd)?;
        w = window::info(window::hwnd(w.hwnd));
    }
    let want = match t.rel {
        Some((rx, ry)) => (w.rect.x + rx, w.rect.y + ry),
        None => (t.x, t.y),
    };
    if !t.by_name || t.name.is_empty() {
        return Ok(Found::At(want.0, want.1));
    }
    let hits = ctx.scanner()?.find_named(w.hwnd, &t.name)?;
    // The same kind if there is one; of several, the one nearest to where it was
    let same: Vec<Rect> = hits.iter().filter(|(k, _)| *k == t.kind).map(|(_, r)| *r).collect();
    let pool = if same.is_empty() { hits.iter().map(|(_, r)| *r).collect() } else { same };
    let far = |p: &(i32, i32)| (p.0 - want.0).pow(2) as i64 + (p.1 - want.1).pow(2) as i64;
    Ok(match pool.iter().map(|r| point_in(r, t.fx, t.fy)).min_by_key(far) {
        Some((x, y)) => Found::At(x, y),
        None => Found::Missing(Msg::new("err.mac_no_element").with("name", &t.name)),
    })
}

fn point_in(r: &Rect, fx: f64, fy: f64) -> (i32, i32) {
    let along = |start: i32, size: i32, f: f64| start + ((size - 1).max(0) as f64 * f.clamp(0.0, 1.0)).round() as i32;
    (along(r.x, r.w, fx), along(r.y, r.h, fy))
}

/// What is at a point of the screen, as a click step will look for it again: the element's name and where in
/// it, the app's window and the point within it. Big areas (documents, panes) are found by position instead.
pub fn target_at(ctx: &mut Ctx, x: i32, y: i32) -> Target {
    let root = window::root_at(x, y);
    let win = window::list().into_iter().find(|w| w.hwnd == root).or_else(|| window::taskbar().filter(|t| t.hwnd == root));
    let mut t = Target {
        name: String::new(),
        kind: String::new(),
        by_name: false,
        fx: 0.5,
        fy: 0.5,
        app: win.as_ref().map(AppRef::of),
        rel: win.as_ref().map(|w| (x - w.rect.x, y - w.rect.y)),
        x,
        y,
    };
    if let Some(e) = ctx.scanner().ok().and_then(|s| s.element_at(x, y).ok().flatten()) {
        let share = |v: i32, start: i32, size: i32| {
            if size > 1 { ((v - start) as f64 / (size - 1) as f64).clamp(0.0, 1.0) } else { 0.5 }
        };
        t.fx = share(x, e.rect.x, e.rect.w);
        t.fy = share(y, e.rect.y, e.rect.h);
        t.by_name = t.app.is_some()
            && !matches!(e.kind, "document" | "pane" | "window" | "group" | "title bar" | "toolbar" | "list" | "menu" | "scrollbar" | "element");
        t.name = e.name;
        t.kind = e.kind.into();
    }
    t
}

/// Adds a step; the same key pressed again becomes one step with a count.
fn add_step(steps: &mut Vec<Step>, act: Act) {
    if let (Act::Key { combo, .. }, Some(Step { act: Act::Key { combo: last, times }, .. })) = (&act, steps.last_mut())
        && combo == last
        && *times < 100
    {
        *times += 1;
        return;
    }
    if steps.len() < MAX_STEPS {
        steps.push(Step { act, go_on: false });
    }
}

/// `back` Backspaces, then `text`, then Enter if `enter`; see `Engine::record_live`.
fn add_live(steps: &mut Vec<Step>, back: u32, text: &str, enter: bool) {
    let mut back = back as usize;
    if back > 0
        && let Some(Step { act: Act::Type { text: last, enter: false }, .. }) = steps.last_mut()
    {
        let n = last.chars().count();
        let keep = n.saturating_sub(back);
        back -= n - keep;
        *last = last.chars().take(keep).collect();
        if keep == 0 {
            steps.pop();
        }
    }
    if back > 0 {
        for _ in 0..back.min(100) {
            add_step(steps, Act::Key { combo: "backspace".into(), times: 1 });
        }
    }
    if !text.is_empty() {
        match steps.last_mut() {
            Some(Step { act: Act::Type { text: last, enter: false }, .. })
                if last.chars().count() + text.chars().count() <= MAX_TEXT =>
            {
                last.push_str(text)
            }
            _ => add_step(steps, Act::Type { text: text.into(), enter: false }),
        }
    }
    if enter {
        match steps.last_mut() {
            Some(Step { act: Act::Type { enter: e @ false, .. }, .. }) => *e = true,
            _ => add_step(steps, Act::Key { combo: "enter".into(), times: 1 }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_in_elements() {
        let r = Rect { x: 100, y: 50, w: 21, h: 11 };
        assert_eq!(point_in(&r, 0.5, 0.5), (110, 55));
        assert_eq!(point_in(&r, 0.0, 1.0), (100, 60));
        assert_eq!(point_in(&r, 2.0, -1.0), (120, 50));
    }

    #[test]
    fn live_typing_is_one_step() {
        let mut steps = Vec::new();
        for (back, text, enter) in [(0, "Mer", false), (0, "habq", false), (1, "a", false), (0, " dünya", false), (0, "", true)] {
            add_live(&mut steps, back, text, enter);
        }
        assert!(matches!(&steps[..], [Step { act: Act::Type { text, enter: true }, .. }] if text == "Merhaba dünya"));
        // Taking back more than was typed presses Backspace for the rest; Enter after a key is a key
        add_live(&mut steps, 0, "ab", false);
        add_live(&mut steps, 4, "", true);
        assert_eq!(steps.len(), 3);
        assert!(matches!(&steps[1].act, Act::Key { combo, times: 2 } if combo == "backspace"));
        assert!(matches!(&steps[2].act, Act::Key { combo, times: 1 } if combo == "enter"));
    }
}
