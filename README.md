# StrCu

Use your Windows computer from your phone. When you are away from the desk, it replaces calling someone at
home and talking them through "open that program, click there".

- See the screen live on the phone; a tap aims a crosshair, then you click, scroll or type.
- Uses Windows' own accessibility data (UI Automation): the name of the element under the crosshair is shown,
  and the elements on screen come as a list. No AI, no GPU, no extra install.
- A single exe. The web panel is protected by a password; a phone can instead be paired once with a QR code,
  and passkeys (fingerprint / Face ID) are an option.

> Work in progress: guided Cloudflare setup and one-line install are on the way to v1.0.

## Build and run

```
cargo build --release
target
elease\strcu.exe
```

`strcu.exe` opens in the terminal. The first start is a short setup: language, panel password (at least 10
characters, argon2), home network, remote access (optional) and starting with Windows (minimized). After that
the main screen shows where the phone can open the panel, the recent important events (sign-ins, failed
attempts, phones paired, fingerprints added or removed, locking, the tunnel coming and going) and four keys:
pair a phone, settings, the full log and quit. Settings change the language, the password (phones are signed
out), the home network, remote access, starting with Windows, paired phones and registered fingerprints, and
the port (8765 by default).

Only one StrCu runs at a time; starting it again brings the running one's window to the front. Closing the
window stops the panel and the tunnel.

```
strcu serve [--bind 127.0.0.1:8765] [--no-tunnel]   # no screens, status lines only (scripts, services)
strcu passwd                      # panel password from the command line
strcu doctor                      # inspect the system, write %LOCALAPPDATA%\strcu\profile.json
```

Developer tools drive the operating system layer directly:

```
strcu dev shot -o screen.jpg --max-side 1280
strcu dev elements [--all]        # visible clickable elements (UI Automation)
strcu dev find "Save" --click     # find by name and click (--double)
strcu dev at 689 919              # element at a point
strcu dev click 689 919 --button right
strcu dev type "Unicode text: şğüöçıİ é ß"
strcu dev key ctrl+shift+esc
strcu dev windows | focus <name> [--maximize] | close <name>
strcu dev apps                    # apps in the Start menu
strcu dev launch "Calculator"     # launch an app (full or partial name)
strcu dev lockstate | lock | awake [--display]
strcu dev shutdown [--secs 15] | shutdown --cancel
strcu dev preview --lang tr       # every terminal screen with made-up data, as HTML pages
```

Coordinates are physical pixels everywhere.

## Panel (phone)

Five tabs: Screen, Keyboard, Elements, Windows, More.

- **Top bar:** the foreground window with its app icon; tap it for the list of open windows. Shows when the
  computer is locked. Next to it, the live view controls (HD / SD / ❚❚).
- **Screen:** a tap aims the crosshair; a bar with Click / Double / Right, ↑ / ↓ (scroll) and ✕ appears next to
  it. Below the image, a magnifier and the element's name. A typing box and eight common keys sit underneath.
- **Full screen:** ⛶ or turn the phone sideways. Pinch to zoom, drag to pan, tap to aim.
- **Live view:** SD scales the long side to 1280 pixels, HD sends full resolution. The image refreshes every
  2.5 s while the Screen tab is open; ❚❚ freezes it.
- **Keyboard:** a text box (types into the foreground window), modifier keys (Ctrl / Alt / Shift / Win, held for
  the next key), common keys, arrows, F1–F12 and recent shortcuts.
- **Elements:** named clickable elements of the foreground window and the taskbar, filterable by name and kind.
- **Windows:** open windows with their app icons; bring to front, maximize or close. **Open app** lists the Start
  menu apps (classic and Store) with search and recent apps.
- **More:** session and sign-out, passkeys, lock and shut down (with a 15 s countdown that can be cancelled),
  panel language and a log of what was done.
- The font (IBM Plex, SIL Open Font License) is embedded; the page loads nothing from outside.

## Languages

English and Turkish. The panel follows the phone's language and can be switched under More; the terminal's
language is chosen in the setup (Windows' display language comes first) and changed in Settings. Every text lives in
`lang/<code>.json`, shared by the panel and the terminal; a language is added by adding a file (the `_name` key
holds its own name, `{name}` placeholders are filled in, `{"one": ..., "other": ...}` gives plural forms). The
tests check that every file has the same keys and placeholders as `lang/en.json`.

## Who can get in

Every request is placed by where its connection really comes from (the socket's addresses and, for one from
this computer, the program that opened it), not only by its headers.

- **This computer:** a browser in the same Windows session gets in without signing in. A connection opened by
  another Windows user signed in to the same computer is refused. Only `localhost` as the address: a site
  that points its own name at 127.0.0.1 is treated as remote (DNS rebinding).
- **Home network** (when turned on): private addresses (192.168.x, 10.x, 172.16-31.x) on a network Windows
  counts as *Private*; on a *Public* network (café, hotel) it switches itself off. The address typed must be
  the computer's IP or its name. Sign-in with the panel password or a paired phone. The connection is plain
  HTTP, so anyone on the same Wi-Fi could read it; browsers offer no passkeys there.
- **Remote:** through the Cloudflare tunnel, below. Connections opened by cloudflared always count as remote.
- **Anything else** is refused, with the reason shown in the visitor's language and written to the log (once a
  minute per address).

Five wrong passwords lock that source out for five minutes; each address has its own count, so a guesser on
the home network cannot lock out remote sign-in. Sign-ins and failed attempts are logged with where they came
from ("Signed in · home network 192.168.1.50").

The port is held for StrCu alone (`SO_EXCLUSIVEADDRUSE`): otherwise, while StrCu listens on 0.0.0.0, another
program could still listen on 127.0.0.1 at the same port and Windows would hand it this computer's
connections, the tunnel's among them.

### Pairing a phone

On the main screen, **[E]** (Turkish) / **[P]** (English) shows a QR code and the same six-digit code. The phone
either scans the QR code, which opens the panel with the code in the part after `#` (never sent to the server
in the address, and removed from it at once), or types the code under "Pair with a code from the computer" on
the sign-in screen (`src/pair.rs`).

- The code is open only while that screen is shown, for five minutes, and works once. Five wrong tries close
  it; a new one is a key press away.
- The paired phone gets an `HttpOnly` cookie that lets it in without the password for 30 days, renewed while it
  is used. It works only at the address it was paired on (home network IP or tunnel hostname); through the
  tunnel Cloudflare Access is still checked first.
- Only a SHA-256 hash of the cookie's secret is kept in `config.json`. A paired phone is removed in Settings → 6,
  or from the phone with "Unpair" under More.

## Remote access: Cloudflare Tunnel + Access

A remotely managed tunnel in the Cloudflare dashboard (e.g. `strcu.example.com -> http://localhost:8765`),
protected by an Access application. No Windows service or administrator rights are needed: StrCu runs
cloudflared as its own child process, watches its connection, starts it again if it stops, and closes it with
the panel (or if StrCu crashes).

Remote access is set up from the terminal (Settings → Remote access): it downloads cloudflared, then asks for
the tunnel token and the Access settings. The same steps exist as commands:

```
strcu tunnel install              # cloudflared 2026.9.3, verified with SHA-256
strcu tunnel token                # the dashboard's token (or the whole command it shows); never printed
strcu tunnel setup --hostname strcu.example.com --team <team>.cloudflareaccess.com --aud <AUD> --email <email>
strcu tunnel status
strcu serve [--no-tunnel]
```

Three gates: Cloudflare Access (email + one-time code) → the panel verifies the Access token on every request
itself (signature, AUD, email) → the panel password or a passkey. Without Access settings the tunnel does not
open at all.

### Passkeys (WebAuthn)

The phone creates a key pair for this site; the private key never leaves the phone and only the public key is
kept in `config.json`. To sign in, the server issues a one-time challenge (valid for 2 minutes) and the phone
signs it after the fingerprint / face / screen lock check (`src/passkey.rs`). The server checks the challenge,
the origin, the site hash, user verification, the ES256 signature and the signature counter.

- A key is bound to the domain: the tunnel hostname, or `localhost` when testing on the computer itself.
- No attestation is requested; adding a key requires the panel password instead.

## Layout

- `src/sys/` operating system layer (screen, input, windows, UI Automation, power, apps, icons, discovery,
  child processes)
- `src/server.rs` + `src/web/` the panel; `src/worker.rs` runs operating system work in order on one thread
- `src/app.rs` what runs while StrCu is open (listener, tunnel watcher); `src/tui/` the terminal screens, plain
  functions from state to frames that the terminal draws and `strcu dev preview` turns into HTML
- `src/auth.rs` sign-in, `src/pair.rs` paired phones, `src/passkey.rs` passkeys, `src/access.rs` Cloudflare
  Access verification
- `src/i18n.rs` + `lang/` languages; the server sends message keys, the panel and the terminal render them
- `src/tunnel.rs` cloudflared management, `src/download.rs` downloads verified with SHA-256

## License

MIT, see [LICENSE](LICENSE). The embedded IBM Plex fonts are under the SIL Open Font License
([src/web/fonts/OFL.txt](src/web/fonts/OFL.txt)).
