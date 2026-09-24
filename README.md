# BucketMount

A small macOS app (Rust + Tauri 2) that mounts S3 buckets as volumes, keeps
them mounted, and tells you from the menu bar when the connection is lost.

- Each bucket appears as a volume in Finder (under **Locations** in the
  sidebar). Files you save are uploaded within seconds; files others upload
  appear within a minute.
- A status dot on the menu bar bucket shows the worst state of all mounts:
  green connected, blue syncing, orange mounting, red disconnected/down, grey
  disabled.
- The `rclone nfsmount` process behind each volume is babysat: if it dies,
  hangs, is ejected, or the bucket becomes unreachable, you get a
  notification and it is restarted with backoff. Nothing needs installing —
  rclone ships inside the app and macOS's built-in NFS client does the mounting
  (no kernel extensions, no macFUSE).
- Everything is driven by one file, `~/.config/bucketmount/config.toml`.
  Copy it to a new Mac, launch the app, and your buckets come back.

## Synced folders

A bucket can also be set up as a **synced folder** instead of a mount
(`mode = "sync"`, or **Synced folder** in the editor). The folder is an
ordinary folder on your disk, so editors, git and file watchers behave exactly
as they do anywhere else, and it works offline. BucketMount keeps it in
two-way sync with the bucket using `rclone bisync`:

- Local changes are synced `write_back_secs` (default 5) after the last change,
  detected through FSEvents.
- The bucket is checked for changes made elsewhere every `dir_cache_secs`
  (default 60, minimum 10).
- The first sync merges the folder and the bucket, keeping the newer copy of
  any file that exists on both sides. After that, a file changed on both sides
  between syncs is kept twice, as `name.conflict1` and `name.conflict2`.
- `.DS_Store`, `._*` AppleDouble files and editor swap files are not synced.
- If more than half the files would be deleted in one run, bisync stops and
  asks you to check (see the mount's log).
- If bisync ever loses track of its state, the folder shows an error with a
  **Resync** button that merges both sides again.

Synced folders need disk space for the whole bucket (or the prefix you choose)
and do not support read-only mode.

## Install

Every tagged version (`v0.1.0`, …) is built by GitHub Actions and published on
the [Releases](https://github.com/koomen/bucketsync/releases) page as a
universal (Apple silicon + Intel) `.dmg` and `.zip`.

**From the Releases page:** download `BucketMount_<version>_universal.dmg`,
open it and drag **BucketMount.app** to `/Applications`.

**With the GitHub CLI** (`brew install gh`, then `gh auth login`), latest release:

```sh
tmp="$(mktemp -d)"
gh release download --repo koomen/bucketsync --pattern 'BucketMount-*.zip' --dir "$tmp"
osascript -e 'quit app "BucketMount"' 2>/dev/null || true   # when upgrading
rm -rf /Applications/BucketMount.app
ditto -x -k "$tmp"/BucketMount-*.zip /Applications
open /Applications/BucketMount.app
```

**From source:** see [Building from source](#building-from-source).

Then:

1. Open it. Because the app is not notarized by Apple, macOS will refuse the
   first launch. Go to **System Settings → Privacy & Security**, scroll down,
   and click **Open Anyway** next to BucketMount, then launch it again.
   (Alternatively: `xattr -dr com.apple.quarantine /Applications/BucketMount.app`.
   Apps installed with `gh` or built locally are not quarantined and open directly.)
2. On first launch it asks whether to start at login. Say yes if you want your
   volumes available whenever you log in. A "Background Items Added"
   notification from macOS is expected; the entry appears under
   **System Settings → General → Login Items**.
3. Click **Add mount**, enter the bucket, region and credentials, hit **Test
   connection**, then **Save**. The volume mounts within a few seconds.

Requirements: macOS 13 or newer, Apple silicon or Intel.

## The two views

**Mount list** — one card per bucket with its state, mount point, an **Open**
button (reveals the volume in Finder) and **Edit**. Below the list: the
start-at-login switch, the rclone version in use, and links to the config and
log folders.

**Mount settings** — name, bucket, optional path inside the bucket, mount
point, provider/region/endpoint, credentials (inline keys, the AWS default
credential chain, or an existing rclone remote), enabled/read-only, and an
**Advanced** section for the upload delay, listing cache, cache size and extra
rclone flags. When editing an existing mount the bottom of the view shows its
live status, restart count, a **Restart mount** button and the tail of its
rclone log.

## Configuration file

`~/.config/bucketmount/config.toml` — see [`config.example.toml`](config.example.toml)
for every field with comments. Minimal example:

```toml
start_at_login = true

[[mount]]
name = "photos"
mode = "mount"                 # or "sync" for a synced local folder
bucket = "my-photos-bucket"
mount_point = "~/BucketMount/photos"
region = "us-west-2"
access_key_id = "AKIA..."
secret_access_key = "..."
```

The UI writes this file; you can also edit it by hand and relaunch the app.
Credentials are stored in plain text with file mode `0600`. If you would
rather not keep keys in the file, use `env_auth = true` (AWS CLI profiles, SSO,
instance roles; pick one with `aws_profile = "name"`) or
`rclone_remote = "name"` to reuse a remote from your own
`~/.config/rclone/rclone.conf`.

**AWS SSO (IAM Identity Center).** Point `aws_profile` at a profile in
`~/.aws/config` that has `sso_session` (or the older `sso_start_url`). The AWS
CLI is not needed: when the session expires the mount shows *Sign-in required*
and a notification is posted; click **Sign in** (mount card, menu bar or the
mount's status panel), approve the code in the browser, and the token is cached
in `~/.aws/sso/cache/` like `aws sso login` would. Mounts re-check right away.
Short-lived access tokens are refreshed automatically in between.

Set `BUCKETMOUNT_CONFIG_DIR` to use a different directory.

## How it works

```
BucketMount.app (Rust core, Tauri 2 window + menu bar item, plain HTML/CSS/JS UI)
 └─ one supervisor thread per mount
     └─ rclone nfsmount  bucket:name  ~/BucketMount/name
        --vfs-cache-mode full  --vfs-write-back 5s  -o locallocks  --rc
```

- **Mounting.** rclone runs a local NFS server for the bucket and mounts it
  with macOS's `mount_nfs`. Reads are streamed and cached on demand; writes
  land in a local cache (`~/Library/Caches/BucketMount/<name>`) and are
  uploaded `write_back_secs` after the last write. `-o locallocks` makes
  `flock()` work on the volume so editors, SQLite, cargo and the like behave.
- **Watching.** Every second the supervisor checks the process and the mount
  table. Every 5 s it queries rclone's remote-control API for the upload queue
  (this drives the *Syncing* state). Every `health_check_interval_secs` it
  performs a real listing request against the bucket; failure flips the mount
  to *Connection lost* and a macOS notification is posted. Checks run more
  often while disconnected so recovery is noticed quickly.
- **Recovering.** If rclone exits, stops answering for 90 s, or the volume is
  ejected, the supervisor force-unmounts any stale volume and restarts with
  exponential backoff (2 s … 60 s, reset after 5 minutes of health). On
  startup, leftovers from a previous run (orphaned rclone processes, stale
  mounts) are cleaned up first.
- **Staying alive.** The login item is a LaunchAgent with
  `KeepAlive.SuccessfulExit = false`, so launchd relaunches the app if it ever
  crashes, but not when you quit it. Quitting (or logout) unmounts all
  volumes cleanly. Only one instance runs at a time.
- **Logs.** `~/Library/Logs/BucketMount/app.log` for the app,
  `<name>.log` for each mount's rclone output (rotated at 10 MB).

## Notes and limitations

- Finder writes `._*` AppleDouble files next to files on network volumes; they
  are harmless but will appear in the bucket.
- S3 has no change notifications, so changes made elsewhere show up after at
  most `dir_cache_secs` (default 60 s). Lower it for faster visibility at the
  cost of more listing requests.
- Two machines writing the same file at the same time will not be merged; the
  last upload wins.
- Mount points must be empty folders. `/Volumes` is not writable without
  root, so the default is `~/BucketMount/<name>`; volumes still appear in the
  Finder sidebar.

## Building from source

Requires Rust (stable), Xcode command line tools and the Tauri CLI
(`cargo install tauri-cli --version "^2" --locked`). No Node.js: the front end
in `ui/` is plain HTML, CSS and JavaScript loaded straight by the WebView.

```sh
git clone git@github.com:koomen/bucketsync.git && cd bucketsync
scripts/build-app.sh                 # universal .app + .dmg + .zip with rclone bundled -> dist/
ARCHS=native scripts/build-app.sh    # quicker, current architecture only
cargo tauri dev                      # run with live-reloading front end
cargo build                          # plain debug build
ditto dist/BucketMount.app /Applications/BucketMount.app   # install your build
```

`scripts/fetch-rclone.sh` downloads the pinned rclone release into `binaries/`
(Tauri's `externalBin` places it inside the bundle as `Contents/MacOS/rclone`).
`build.rs` runs it automatically when the binary for the target is missing, so
plain `cargo build` works on a fresh checkout. The build script fetches rclone, runs
`cargo tauri build`, ad-hoc signs the result when no Developer ID is
configured, and collects the outputs. If the checkout lives on a network mount
(cargo cannot take file locks there) it mirrors the sources to
`~/.cache/bucketmount/src` first and puts `dist/` there too.

`.github/workflows/release.yml` does the same on a GitHub runner whenever a
`v*` tag is pushed and attaches the zip and dmg to a release. Add the
`APPLE_CERTIFICATE`, `APPLE_CERTIFICATE_PASSWORD`, `APPLE_SIGNING_IDENTITY`,
`APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID` secrets and the Tauri bundler
signs and notarizes the build so it opens without the Gatekeeper detour.

Layout: `src/` Rust (config, rclone, supervisor, mac helpers, Tauri commands
and tray), `ui/` front end, `tauri.conf.json` window/bundle settings,
`capabilities/` Tauri permissions, `icons/` app icon (regenerate from
`assets/icon.png` with `scripts/gen-icon.py` + `iconutil`).

## License

MIT
