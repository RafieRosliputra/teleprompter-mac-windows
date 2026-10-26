# JohorStudio Teleprompter

Two-screen presidential teleprompter written in Rust (egui). macOS now, Windows later.

- **Main screen (operator):** left 2/3 = script (click a line to glide it to the reading line), right 1/3 = live preview of screen 2 + controls.
- **Screen 2 (prompter):** black background, white text, mirrored for the glass, reading line at 1/3 from the top with red cursors left and right.
- **Keys:** `Space` play at default speed / pause · `→` faster · `←` slower / reverse · `↑`/`↓` one line · `Home` top · `Esc` stop.
- Scripts can be loaded from a `.txt` file or from **tele.johorstudio.com** with a 6-digit code.

## Download (Mac)

Get `JohorStudio-Teleprompter-mac-arm64.zip` from [Releases](../../releases), unzip, move the app to Applications.
The app is not notarized: the first time, right-click the app → **Open** → **Open**
(or run `xattr -dr com.apple.quarantine "/Applications/JohorStudio Teleprompter.app"`).

## Build

```sh
cargo build --release              # binary in target/release
./scripts/make-mac-app.sh install  # build .app into dist/ and copy to /Applications
```

## Script server (tele.johorstudio.com)

`server/` is a small Python (stdlib only) web app + SQLite: the public submits scripts and gets a code by email;
the Mac app lists/loads them with the admin token. It runs as the TrueNAS custom app `teleprompter`
(see `deploy/truenas-app.yaml`, data in `/mnt/ssd/teleprompter`), published through the Cloudflare tunnel.
