# vibeseek

Soulseek from the terminal. A CLI + TUI frontend for [slskd](https://github.com/slskd/slskd).

- **Search** the network, with quality filters (lossless-first by default, like your sockseek config)
- **Download** single files or whole folders into any folder you choose
- **CSV batch mode**: Spotify exports and sockseek-style lists, resumable, with automatic retry from other sources
- **Upload monitoring**: who's downloading from you, live progress and speed, permanent history and stats, cancel and ban

```
vibeseek                      # full-screen TUI (tabs: Search / Downloads / Uploads / History)
vibeseek search take five     # one-off search, numbered results
vibeseek get 1 3-5 -o ~/Music/jazz --wait
vibeseek search -a miles davis kind of blue   # group by folder (albums); `get 1` downloads the album
vibeseek uploads -w           # live view of people downloading from you
vibeseek history --since 7d   # upload stats: top downloaders, most-downloaded files, recent
vibeseek csv ~/Desktop/liked_songs.csv -o ~/Music/liked --fallback lossy-ok
```

## How it fits together

```
 vibeseek (CLI / TUI) ──HTTP──▶ slskd (systemd user service) ──▶ Soulseek network
        │                          ▲
        └── vibeseek agent ────────┘  keeps the VPN port in sync, records upload history,
            (systemd user service)    moves finished downloads into custom folders
```

| Thing | Where |
|---|---|
| vibeseek binary | `~/.local/bin/vibeseek` |
| vibeseek config | `~/.config/vibeseek/config.toml` (`vibeseek config --edit`) |
| slskd binary | `~/.local/opt/slskd/` (v0.26.0) |
| slskd config | `~/.local/share/slskd/slskd.yml` (credentials, shares, port, bans) |
| slskd service | `systemctl --user … slskd` or `vibeseek daemon start/stop/restart/status/logs` |
| agent service | `vibeseek-agent.service` (`vibeseek agent install/uninstall`) |
| upload history | `~/.local/share/vibeseek/history.db` |
| CSV progress | `~/.local/share/vibeseek/csv/` |
| slskd web UI | http://127.0.0.1:5030 (localhost only; login is in slskd.yml) |

## Search and download

`vibeseek search <words>` prints numbered results and remembers them, so `vibeseek get <numbers>` downloads them. You can also do both at once with `-d 1 -o <folder>`.

- `-p lossless | lossy-ok | any` picks a filter preset, and `-f flac,mp3`, `--min-bitrate`, `--min-bitdepth`, `--min-samplerate`, `--strict` override it
- `-a` groups results by folder, so `get` downloads the entire remote folder, cover art included
- `-o <folder>` downloads anywhere; without it, files go to `~/Music/downloads/<source folder>/`
- Ranking prefers hi-res FLAC (your sockseek `pref-*` values, in `[prefs]` in config.toml), peers with a free slot and short queue, and filenames that match your words

In the TUI, press `/` to search and `Enter` to download. `v` toggles the files/folders view, `f` cycles filters, and `o` sets the output folder. Press `?` for all keys.

## CSV batch mode

```
vibeseek csv liked_songs.csv                  # → ~/Music/downloads/liked_songs/
vibeseek csv liked_songs.csv --dry-run -n 20  # see what it would pick
vibeseek csv liked_songs.csv --status         # progress + list of what wasn't found
vibeseek csv liked_songs.csv --retry --fallback lossy-ok
```

- Columns are auto-detected (`title/track/name`, `artist/artists`, `album`, `length/duration/duration_ms`). You can override them with `--title-col` etc.
- Rows with only an album become album downloads (whole folder into `Artist - Album/`), and `--albums` forces that for every row.
- A match needs every title word in the filename, the artist somewhere in the path, and a length within 3s. Remixes and live versions are skipped unless the title asks for them.
- A failed or stuck transfer (queued for 15 minutes without starting) moves on to the next best source, up to 3 tries.
- It's safe to Ctrl-C and rerun: finished rows, and files already in the output folder, are skipped.
- Soulseek bans clients that search too fast, so it stays under 34 searches per 220s. A 2,500-row CSV takes about 4.5 hours.

## Uploads

`vibeseek uploads` shows active and queued uploads (`-w` for a live view, `--all` for finished ones too). `vibeseek uploads cancel 2`, `… cancel user:NAME`, `vibeseek ban NAME`, and `vibeseek uploads clear` manage them.

slskd forgets finished transfers after a while, so the agent (and the TUI while it's open) records every finished upload into `history.db`. `vibeseek history` shows totals, top downloaders, most-downloaded files, and recent uploads. `-u NAME` shows one person's uploads.

## VPN port

ProtonVPN assigns a new forwarded port on every connect. The agent asks the VPN gateway (`natpmpc -g 10.2.0.1`) every 45s and writes the port into slskd.yml, which slskd applies live without a restart.

- `vibeseek port --show` compares the current port with the VPN's
- `vibeseek port` syncs it right now
- `vibeseek port 40649` sets it by hand

## Using sockseek / Nicotine+ too

Soulseek allows one login per account, and slskd holds it while it's running. Run `vibeseek daemon stop` before logging in to the same account with sockseek or Nicotine+, and `vibeseek daemon start` afterwards.
