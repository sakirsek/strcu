//! Languages. One JSON file per language in `lang/` holds every text of the panel and the terminal;
//! `build.rs` embeds all of them, so adding a language means adding a file.
//!
//! Texts are looked up by key. `{name}` placeholders are filled from arguments, and a value can be an object
//! of plural forms (`{"one": ..., "other": ...}`) chosen by the `n` argument. Keys a language lacks fall back
//! to English.
//!
//! The server sends the panel no finished sentences: log entries and errors are `Msg` values (a key plus
//! arguments) that the panel and the terminal each render in their own language.

use std::fmt;
use std::sync::{LazyLock, RwLock};

use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::{Map, Value};

include!(concat!(env!("OUT_DIR"), "/lang_files.rs"));

pub struct Lang {
    pub code: &'static str,
    /// The language's name in itself ("Türkçe"), from the `_name` key
    pub name: String,
    dict: Map<String, Value>,
}

static LANGS: LazyLock<Vec<Lang>> = LazyLock::new(|| {
    FILES
        .iter()
        .map(|&(code, src)| {
            let dict: Map<String, Value> =
                serde_json::from_str(src).unwrap_or_else(|e| panic!("lang/{code}.json: {e}"));
            let name = dict.get("_name").and_then(Value::as_str).unwrap_or(code).to_string();
            Lang { code, name, dict }
        })
        .collect()
});

pub fn all() -> &'static [Lang] {
    &LANGS
}

pub fn get(code: &str) -> Option<&'static Lang> {
    LANGS.iter().find(|l| l.code.eq_ignore_ascii_case(code))
}

pub fn en() -> &'static Lang {
    get("en").expect("lang/en.json is missing")
}

/// A key as a lasting string. The language files are built into StrCu and live as long as it runs, so their
/// own keys are what a `Msg` can keep. It is for a key that arrived as text - from the agent, which cannot
/// send a `Msg` of its own (agent.rs) - and None for a key no file has, so a made-up one never becomes text.
pub fn code(key: &str) -> Option<&'static str> {
    LANGS.iter().find_map(|l| l.dict.get_key_value(key)).map(|(k, _)| k.as_str())
}

/// The placeholder names in a key's English text ("path" for "Could not open {path}"), as lasting strings.
/// An argument the text does not name changes nothing when it is shown, so this is also the list of what a
/// key may be given.
pub fn args_of(key: &str) -> Vec<&'static str> {
    let texts: Vec<&'static str> = match en().lookup(key) {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Object(forms)) => forms.values().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let mut out = Vec::new();
    for text in texts {
        let mut rest = text;
        while let Some(i) = rest.find('{') {
            rest = &rest[i + 1..];
            let Some(j) = rest.find('}') else { break };
            let name = &rest[..j];
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
            rest = &rest[j + 1..];
        }
    }
    out
}

/// First supported language among language tags ("tr-TR", "en"), by full code or primary subtag.
pub fn pick<'a>(tags: impl IntoIterator<Item = &'a str>) -> Option<&'static Lang> {
    tags.into_iter().find_map(|t| {
        let t = t.trim();
        get(t).or_else(|| t.split(['-', '_']).next().and_then(get))
    })
}

/// Best language for an `Accept-Language` header ("tr-TR,tr;q=0.9,en;q=0.8").
pub fn from_accept_language(header: &str) -> Option<&'static Lang> {
    let mut tags: Vec<(f32, &str)> = header
        .split(',')
        .filter_map(|part| {
            let mut it = part.split(';');
            let tag = it.next()?.trim();
            let q = it.find_map(|p| p.trim().strip_prefix("q=")).and_then(|q| q.parse().ok()).unwrap_or(1.0);
            (!tag.is_empty() && tag != "*" && q > 0.0).then_some((q, tag))
        })
        .collect();
    // Stable sort: equal weights keep the header's order
    tags.sort_by(|a, b| b.0.total_cmp(&a.0));
    pick(tags.into_iter().map(|(_, t)| t))
}

/// Terminal language: the one chosen in the settings, else Windows' display language, else English.
pub fn term() -> &'static Lang {
    if let Some(l) = *TERM.read().unwrap() {
        return l;
    }
    let l = crate::config::load().language.as_deref().and_then(get).unwrap_or_else(system);
    set_term(l);
    l
}

static TERM: RwLock<Option<&'static Lang>> = RwLock::new(None);

/// Switches the terminal language (after a choice in the setup or the settings).
pub fn set_term(l: &'static Lang) {
    *TERM.write().unwrap() = Some(l);
}

/// Windows' display language if there is a file for it, else English.
pub fn system() -> &'static Lang {
    pick([crate::sys::discover::ui_language().as_str()]).unwrap_or_else(en)
}

impl Lang {
    fn lookup(&self, key: &str) -> Option<&Value> {
        self.dict.get(key).or_else(|| en().dict.get(key))
    }

    /// Text of a key, filled with arguments. An unknown key comes back as is.
    pub fn text(&self, key: &str, args: &[(&'static str, Arg)]) -> String {
        let tpl = match self.lookup(key) {
            Some(Value::String(s)) => s.as_str(),
            Some(Value::Object(forms)) => {
                let one = args.iter().any(|(k, v)| *k == "n" && *v == Arg::Num(1));
                forms.get(if one { "one" } else { "other" }).or_else(|| forms.get("other")).and_then(Value::as_str).unwrap_or(key)
            }
            _ => key,
        };
        self.fill(tpl, args)
    }

    pub fn t(&self, key: &str) -> String {
        self.text(key, &[])
    }

    pub fn render(&self, m: &Msg) -> String {
        self.text(m.code, &m.args)
    }

    fn fill(&self, tpl: &str, args: &[(&'static str, Arg)]) -> String {
        let mut out = String::with_capacity(tpl.len());
        let mut rest = tpl;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let found = after.find('}').and_then(|j| {
                let name = &after[..j];
                args.iter().find(|(k, _)| *k == name).map(|(_, v)| (name, v, j))
            });
            match found {
                Some((name, v, j)) => {
                    match v {
                        Arg::Text(s) if name == "combo" => out.push_str(&self.combo(s)),
                        Arg::Text(s) => out.push_str(s),
                        Arg::Num(n) => out.push_str(&n.to_string()),
                        Arg::Msg(m) => out.push_str(&self.render(m)),
                    }
                    rest = &after[j + 1..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// Readable key combination: "ctrl+shift+esc" -> "Ctrl+Shift+Esc", names from the `key.*` texts.
    pub fn combo(&self, combo: &str) -> String {
        combo
            .split('+')
            .map(|p| {
                let p = p.trim().to_lowercase();
                match self.lookup(&format!("key.{p}")).and_then(Value::as_str) {
                    Some(name) => name.to_string(),
                    None => {
                        let mut c = p.chars();
                        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
                    }
                }
            })
            .collect::<Vec<_>>()
            .join("+")
    }

    /// All texts with English filling the gaps, as JSON that is safe inside a `<script>` element.
    pub fn page_json(&self) -> String {
        let mut dict = en().dict.clone();
        dict.extend(self.dict.iter().map(|(k, v)| (k.clone(), v.clone())));
        Value::Object(dict).to_string().replace("</", "<\\/")
    }
}

/// A text to be shown: a language key and its arguments. Also usable as an error, so code deep inside can
/// report something the user reads in their own language.
#[derive(Debug, Clone, PartialEq)]
pub struct Msg {
    pub code: &'static str,
    pub args: Vec<(&'static str, Arg)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Arg {
    Text(String),
    Num(i64),
    Msg(Msg),
}

impl Msg {
    pub fn new(code: &'static str) -> Self {
        Msg { code, args: Vec::new() }
    }

    pub fn with(mut self, name: &'static str, value: impl Into<Arg>) -> Self {
        self.args.push((name, value.into()));
        self
    }
}

impl From<String> for Arg {
    fn from(s: String) -> Self {
        Arg::Text(s)
    }
}
impl From<&str> for Arg {
    fn from(s: &str) -> Self {
        Arg::Text(s.to_string())
    }
}
impl From<&String> for Arg {
    fn from(s: &String) -> Self {
        Arg::Text(s.clone())
    }
}
impl From<Msg> for Arg {
    fn from(m: Msg) -> Self {
        Arg::Msg(m)
    }
}
macro_rules! num_arg {
    ($($t:ty),*) => {$(
        impl From<$t> for Arg {
            fn from(n: $t) -> Self {
                Arg::Num(n as i64)
            }
        }
    )*};
}
num_arg!(u16, i32, i64, u32, u64, usize);

/// A `Msg` written down: the key as text with its arguments beside it, for the places a `Msg` itself cannot
/// go. Its key is a `&'static str`, so nothing that arrives at runtime can build one; a key that comes back
/// finds its `Msg` again among the texts StrCu was built with (`code`). A key no language file has, or an
/// argument its text does not name, is left out, and what is left to show is "something went wrong" with the
/// key behind it. It travels the agent's two pipes (agent.rs) and the file the service leaves its log in
/// (log.rs).
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Clone)]
pub struct Written {
    code: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<(String, Said)>,
}

/// An argument of such a text. A number stays a number, because that is what chooses between "1 file" and
/// "5 files".
#[derive(serde::Serialize, serde::Deserialize, Debug, PartialEq, Clone)]
#[serde(untagged)]
pub enum Said {
    Num(i64),
    Text(String),
}

impl Written {
    /// What went wrong, from the error something gave up with.
    pub fn of(e: &anyhow::Error) -> Written {
        Written::told(&from_error(e))
    }

    pub fn told(m: &Msg) -> Written {
        let args = m
            .args
            .iter()
            .map(|(name, value)| {
                let said = match value {
                    Arg::Num(n) => Said::Num(*n),
                    Arg::Text(t) => Said::Text(t.clone()),
                    // A text inside a text: in English, which is what such a detail is anyway
                    Arg::Msg(inner) => Said::Text(inner.to_string()),
                };
                (name.to_string(), said)
            })
            .collect();
        Written { code: m.code.to_string(), args }
    }

    /// The text to show, for whoever renders it in their own language.
    pub fn msg(&self) -> Msg {
        let Some(code) = code(&self.code) else {
            return Msg::new("err.detail").with("msg", Msg::new("err.internal")).with("detail", self.code.clone());
        };
        let names = args_of(code);
        let mut m = Msg::new(code);
        for (name, said) in &self.args {
            if let Some(&name) = names.iter().find(|n| **n == *name) {
                m = m.with(name, match said {
                    Said::Num(n) => Arg::Num(*n),
                    Said::Text(t) => Arg::Text(t.clone()),
                });
            }
        }
        m
    }
}

impl Serialize for Msg {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("code", self.code)?;
        if !self.args.is_empty() {
            m.serialize_entry("args", &Args(&self.args))?;
        }
        m.end()
    }
}

struct Args<'a>(&'a [(&'static str, Arg)]);

impl Serialize for Args<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

impl Serialize for Arg {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Arg::Text(t) => s.serialize_str(t),
            Arg::Num(n) => s.serialize_i64(*n),
            Arg::Msg(m) => m.serialize(s),
        }
    }
}

/// In English, for logs and developer output; users see `Lang::render`.
impl fmt::Display for Msg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&en().render(self))
    }
}

impl std::error::Error for Msg {}

/// The message for an error: its `Msg` (plus the underlying cause, if the `Msg` was only context), or
/// "something went wrong" with the English detail.
pub fn from_error(e: &anyhow::Error) -> Msg {
    match e.downcast_ref::<Msg>() {
        Some(m) => {
            let root = e.root_cause();
            // Alone it is the whole story; given to a missing value as context it is the only link too
            if root.downcast_ref::<Msg>().is_some() || e.chain().count() == 1 {
                m.clone()
            } else {
                Msg::new("err.detail").with("msg", m.clone()).with("detail", root.to_string())
            }
        }
        None => Msg::new("err.detail").with("msg", Msg::new("err.internal")).with("detail", format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use anyhow::Context;

    use super::*;

    #[test]
    fn a_text_keeps_its_meaning_when_it_is_written_down() {
        let said = Written::told(&Msg::new("err.open_failed").with("path", r"C:\Users\me\rapor.xlsx"));
        let read: Written = serde_json::from_str(&serde_json::to_string(&said).unwrap()).unwrap();
        assert_eq!(read, said);
        assert_eq!(en().render(&read.msg()), r"Could not open C:\Users\me\rapor.xlsx");
        // A number stays a number, so the plural forms still work
        let counted = Written::told(&Msg::new("ev.typed").with("n", 1));
        assert_eq!(en().render(&counted.msg()), "Typed 1 character");
        // A key this StrCu does not have cannot become a text of its own
        // Built, not written: a quoted key in our own source is a key the language files must have
        let made_up = Written { code: ["err", "from_the_future"].join("."), args: Vec::new() };
        assert_eq!(en().render(&made_up.msg()), "Something went wrong: err.from_the_future");
        // An error without a text of its own arrives as the detail behind "something went wrong"
        let boom = Written::of(&anyhow::anyhow!("boom"));
        assert_eq!(en().render(&boom.msg()), "Something went wrong: boom");
    }

    fn placeholders(v: &Value) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut add = |s: &str| {
            let mut rest = s;
            while let Some(i) = rest.find('{') {
                rest = &rest[i + 1..];
                if let Some(j) = rest.find('}') {
                    out.insert(rest[..j].to_string());
                    rest = &rest[j + 1..];
                }
            }
        };
        match v {
            Value::String(s) => add(s),
            Value::Object(forms) => forms.values().filter_map(Value::as_str).for_each(&mut add),
            _ => {}
        }
        out
    }

    #[test]
    fn languages_match_english() {
        let en = en();
        assert!(all().len() >= 2);
        for l in all() {
            assert!(l.dict.get("_name").is_some_and(Value::is_string), "{}: _name", l.code);
            let missing: Vec<_> = en.dict.keys().filter(|k| !l.dict.contains_key(*k)).collect();
            let extra: Vec<_> = l.dict.keys().filter(|k| !en.dict.contains_key(*k)).collect();
            assert!(missing.is_empty(), "{}: missing {missing:?}", l.code);
            assert!(extra.is_empty(), "{}: unknown keys {extra:?}", l.code);
            for (k, v) in &l.dict {
                assert!(v.is_string() || v.as_object().is_some_and(|o| o.contains_key("other")), "{}: {k}", l.code);
                let (mine, theirs) = (placeholders(v), placeholders(&en.dict[k]));
                assert!(mine.is_subset(&theirs), "{}: {k} uses {mine:?}, English has {theirs:?}", l.code);
            }
        }
    }

    /// Quoted strings in source code that look like a key of one of the files' sections ("err.focus_failed").
    fn quoted_keys(src: &str, sections: &BTreeSet<&str>, out: &mut BTreeSet<String>) {
        let b = src.as_bytes();
        for (i, &q) in b.iter().enumerate() {
            if q != b'"' && q != b'\'' {
                continue;
            }
            let body = b[i + 1..].iter().take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || **c == b'_' || **c == b'.');
            let n = body.count();
            if b.get(i + 1 + n) != Some(&q) || n == 0 {
                continue;
            }
            let key = &src[i + 1..i + 1 + n];
            let section = key.split('.').next().unwrap_or_default();
            if key.contains('.') && !key.ends_with(['.', '_']) && sections.contains(section) {
                out.insert(key.to_string());
            }
        }
    }

    /// Every key the code uses exists, and every key in the files is used.
    #[test]
    fn keys_are_used_and_defined() {
        let en = en();
        let sections: BTreeSet<&str> = en.dict.keys().filter_map(|k| k.split_once('.')).map(|(s, _)| s).collect();
        let mut used = BTreeSet::new();
        let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs" || x == "html") {
                    quoted_keys(&std::fs::read_to_string(&p).unwrap(), &sections, &mut used);
                }
            }
        }
        let undefined: Vec<_> = used.iter().filter(|k| !en.dict.contains_key(*k)).collect();
        assert!(undefined.is_empty(), "keys used but not in lang/en.json: {undefined:?}");
        // Keys built at run time ('kind.' + kind and so on)
        const DYNAMIC: &[&str] = &["kind.", "key.", "group.", "act.done_"];
        let unused: Vec<_> = en
            .dict
            .keys()
            .filter(|k| *k != "_name" && !used.contains(*k) && !DYNAMIC.iter().any(|p| k.starts_with(p)))
            .collect();
        assert!(unused.is_empty(), "keys in lang/en.json that nothing uses: {unused:?}");
    }

    #[test]
    fn rendering() {
        let (en, tr) = (get("en").unwrap(), get("tr").unwrap());
        let m = Msg::new("ev.typed").with("n", 1);
        assert_eq!(en.render(&m), "Typed 1 character");
        assert_eq!(en.render(&Msg::new("ev.typed").with("n", 5)), "Typed 5 characters");
        assert_eq!(tr.render(&Msg::new("ev.typed").with("n", 5)), "5 karakter yazıldı");
        let nested = Msg::new("ev.titled").with("action", Msg::new("ev.front")).with("title", "Notepad");
        assert_eq!(en.render(&nested), "Brought to front: Notepad");
        assert_eq!(tr.render(&nested), "Öne getirildi: Notepad");
        assert_eq!(en.render(&Msg::new("ev.key").with("combo", "ctrl+shift+esc")), "Key sent: Ctrl+Shift+Esc");
        assert_eq!(tr.combo("win+space"), "Win+Boşluk");
        assert_eq!(en.combo("alt+f4"), "Alt+F4");
        let unknown = ["no", "such", "key"].join(".");
        assert_eq!(en.t(&unknown), unknown);
        // A placeholder without an argument stays visible
        assert_eq!(en.render(&Msg::new("ev.titled")), "{action}: {title}");
        assert_eq!(
            serde_json::to_string(&nested).unwrap(),
            r#"{"code":"ev.titled","args":{"action":{"code":"ev.front"},"title":"Notepad"}}"#
        );
    }

    #[test]
    fn a_key_that_came_back_as_text() {
        assert_eq!(code("err.no_app"), Some("err.no_app"));
        // Built, not written: a quoted key in our own source is one the language files must have
        assert_eq!(code(&["err", "no", "such", "key"].join(".")), None);
        assert_eq!(args_of("err.open_failed"), ["path"]);
        assert_eq!(args_of("err.detail"), ["msg", "detail"]);
        assert!(args_of("err.internal").is_empty());
        // Both plural forms of a text are looked at
        assert_eq!(args_of("ev.typed"), ["n"]);
        assert!(args_of("no.such.key").is_empty());
    }

    #[test]
    fn language_choice() {
        let code = |l: Option<&Lang>| l.map(|l| l.code);
        assert_eq!(code(from_accept_language("tr-TR,tr;q=0.9,en-US;q=0.8,en;q=0.7")), Some("tr"));
        assert_eq!(code(from_accept_language("de-DE,en;q=0.5,tr;q=0.4")), Some("en"));
        assert_eq!(code(from_accept_language("en;q=0.5, tr")), Some("tr"));
        assert_eq!(code(from_accept_language("de-DE, fr")), None);
        assert_eq!(code(from_accept_language("")), None);
        assert_eq!(code(pick(["TR-tr"])), Some("tr"));
    }

    #[test]
    fn errors_carry_messages() {
        let e: anyhow::Error = Msg::new("err.focus_failed").into();
        assert_eq!(from_error(&e), Msg::new("err.focus_failed"));
        let e = std::fs::read("/no/such/file").context(Msg::new("err.config_read")).unwrap_err();
        let m = from_error(&e);
        assert_eq!(m.code, "err.detail");
        assert_eq!(m.args[0], ("msg", Arg::Msg(Msg::new("err.config_read"))));
        let m = from_error(&anyhow::anyhow!("boom"));
        assert_eq!(en().render(&m), "Something went wrong: boom");
        let wrapped = Msg::new("err.unknown_key").with("key", "x");
        let e = anyhow::Error::from(wrapped.clone()).context("outer");
        assert_eq!(from_error(&e), wrapped);
        // A missing value with a message: the message alone, without an English copy of it
        let e = None::<u8>.context(Msg::new("err.no_app")).unwrap_err();
        assert_eq!(from_error(&e), Msg::new("err.no_app"));
        let page = en().page_json();
        assert!(!page.contains("</"));
    }
}
