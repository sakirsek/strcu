# StrCu

Use your Windows computer from your phone. When you are away from the desk, it replaces calling someone at
home and talking them through "open that program, click there".

- See the screen live on the phone; a tap aims a crosshair, then you click, scroll or type.
- Uses Windows' own accessibility data (UI Automation): the name of the element under the crosshair is shown,
  and the elements on screen come as a list. No AI, no GPU, no extra install.
- A single exe. The web panel is protected by a password, with passkeys (fingerprint / Face ID) as an option.

> Work in progress: setup wizard, home network access, QR pairing, guided Cloudflare setup and one-line install
> are on the way to v1.0.

## Build and run (command line for now)

```
cargo build --release
strcu passwd                      # panel password (at least 10 characters, argon2)
strcu serve                       # web panel: http://127.0.0.1:8765 (+ tunnel if set up)
strcu doctor                      # inspect the system, write %LOCALAPPDATA%\strcu\profile.json
```

Started without arguments (double-click), `strcu.exe` runs `strcu serve`: the panel and the tunnel open, and
closing the window stops both. If startup fails the window stays open until Enter is pressed.

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

English and Turkish. The panel follows the phone's language and can be switched under More; the terminal follows
Windows' display language (or `"language"` in `config.json`), falling back to English. Every text lives in
`lang/<code>.json`, shared by the panel and the terminal; a language is added by adding a file (the `_name` key
holds its own name, `{name}` placeholders are filled in, `{"one": ..., "other": ...}` gives plural forms). The
tests check that every file has the same keys and placeholders as `lang/en.json`.

## Remote access: Cloudflare Tunnel + Access

A remotely managed tunnel in the Cloudflare dashboard (e.g. `strcu.example.com -> http://localhost:8765`),
protected by an Access application. No Windows service or administrator rights are needed: `strcu serve`
runs cloudflared as its own child process, and the tunnel closes with the panel (or if strcu crashes).

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
- `src/auth.rs` sign-in, `src/passkey.rs` passkeys, `src/access.rs` Cloudflare Access verification
- `src/i18n.rs` + `lang/` languages; the server sends message keys, the panel and the terminal render them
- `src/tunnel.rs` cloudflared management, `src/download.rs` downloads verified with SHA-256

## License

MIT, see [LICENSE](LICENSE). The embedded IBM Plex fonts are under the SIL Open Font License
([src/web/fonts/OFL.txt](src/web/fonts/OFL.txt)).
