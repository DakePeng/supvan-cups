# macOS

On macOS the driver ships as a small menu-bar app, **SupvanPrinter.app**. It
bundles the same IPP Everywhere service that runs on Linux and adds what a
background daemon can't give you on a Mac: a status icon, notifications when
a job is held or fails, a Disconnect button for a printer your phone also
uses, and a real app bundle for the Bluetooth privacy prompt.

While the app is running, the printer is an ordinary system printer: pick it
in any app's Print dialog. No vendor app involved.

## Install

Prerequisites: Xcode command-line tools and a Rust toolchain
(`brew install rustup && rustup default stable`).

```sh
make deploy        # build target/SupvanPrinter.app → ~/Applications, open it
```

`make app` only builds; `make uninstall-user` removes the app (the CUPS queue
stays). On first launch:

1. macOS asks whether **Supvan Printer** may use Bluetooth. Allow it — the
   service is not started until this is answered.
2. The app finds the paired printer and adds a CUPS queue for it
   (`lpadmin -m everywhere`, pointing at `ipp://localhost:8631/…`). This
   happens once; remove the queue later and it stays removed, with an **Add
   to system printers** item in the menu.
3. Optionally tick **Open at login** in the menu.

Pair the printer first in System Settings → Bluetooth (its name is its serial,
e.g. `T0200A2501010001`). USB needs nothing: plug it in.

## Using it

| Icon | Meaning |
|------|---------|
| outline printer | not connected — it will connect when you print |
| filled printer | Bluetooth link is up |
| warning triangle | something needs you: label not seated, cover open, out of labels, no Bluetooth permission |

The menu shows the loaded label size and labels remaining, **Connect and
refresh** / **Disconnect**, the CUPS queue name, and the log.

**The link is on demand.** The app does not hold the printer: it dials when a
job arrives (or when you choose Connect), and hangs up after 90 s without
use, before the Mac sleeps, and when you choose Disconnect or quit. The
printer accepts one host at a time, so this is what lets the phone app use
it in between. Change the idle time with
`defaults write com.supvan.printer-menubar idleDisconnectSeconds -int 300`.

**Errors are reported, and the job waits.** A clearable condition — printer
off or held by another host, label not seated, cover open, out of labels —
holds the job, shows a notification saying what to do, and prints
automatically once it is fixed. If notifications are switched off for the
app, the same message appears in a window instead.

**Content and darkness.** The printer makes only black dots, so anything
grey has to be turned into a pattern. The menu's **Content** setting picks
how: *Text and codes* (the default) prints plain black and white — every
pixel darker than mid-grey is ink — which keeps edges crisp; *Photos* uses a
halftone so tones survive. The halftone is what breaks small QR codes: a
code with modules under about three dots is mostly anti-aliasing by the time
macOS has rasterised it, and a halftone turns that into a checkerboard.
Measured on a 35-module code through the mock: at 0.34 mm per module
(2.7 dots) the halftone result does not decode and the black-and-white one
does; at 0.71 mm both do.

**Density** is the vendor app's own scale, 1 (lightest) to 9, default 4 —
the app translates it to the service's 0–100 darkness. Two things learned
the hard way on a T50 Pro: the service's stock default (darkness 50) lands
on density 8, twice the vendor's, which is heavy for small codes; and the
printer takes the density **once per label** — a label printed in bands of
1 to 15 came out uniform, while three labels at 1, 5 and 9 differed
plainly. So compare densities across labels, never within one; **Print a
density comparison** in the Density menu prints those three, each with its
number, sample text, a fine-line grating and a small QR code. **Show the
last print as sent** opens the dots of the most recent job exactly as they
went to the printer.

`open supvanprinter://connect` and `open supvanprinter://disconnect` do what
the menu items do, for Shortcuts and scripts.

**Label size and gap.** A genuine roll carries an RFID tag: the printer reads
width, length and gap from it and finds each label with its gap sensor, so
there is nothing to set — the menu's **Label settings…** says so, and the
app never overwrites such a roll. For stock without a tag, **Label
settings…** takes width (≤ 50 mm), length (≤ 120 mm) and gap, and writes
them to the printer as a material record (`SET_RFID_DATA`), which is what
the vendor app's custom-label screen does. Either way the loaded label
becomes the queue's default paper size, refreshed whenever the link is up.
The print head is 48 mm wide, so on a 50 mm roll about 1 mm each side cannot
be printed. For a single job you can still pick another paper size in the
Print dialog; the page is scaled to fit and centred.

## How it reaches the hardware

| Transport | Linux | macOS |
|-----------|-------|-------|
| USB | `/dev/hidrawN` (needs the udev rule) | `hidapi` over IOHIDManager |
| Classic Bluetooth (SPP) | raw RFCOMM socket, auto-pairs via BlueZ | IOBluetooth RFCOMM channel to a **paired** printer |
| BLE (E11/E12-class) | `bluer`, feature `ble` | not available; the feature compiles to nothing |
| Battery level | BlueZ `BatteryProvider1` | not available |

Linux gets an RFCOMM socket from BlueZ. macOS has no socket API for Classic
Bluetooth; IOBluetooth is the supported way to open an RFCOMM channel.
CoreBluetooth is BLE only, and the T-series print over Classic SPP.

macOS also publishes a paired SPP printer as `/dev/cu.<Name>`. That port is
not used by default: on macOS 26 it opens without bringing the link up and
writes vanish. It works as an explicit CLI target when the link happens to be
up, and `SUPVAN_SERIAL_PORTS=/dev/cu.X` adds such ports to discovery.

## The CLI on macOS

`supvan-cli discover` lists a paired printer as a `BT` row with its address,
and `supvan-cli probe AA:BB:…` talks to it. A bare executable has no bundle
for macOS to attach the Bluetooth permission to, so the terminal you run it
from must itself be allowed under Privacy & Security → Bluetooth (the
binaries embed the usage description this needs). If a first Bluetooth call
just hangs, that permission is what it is waiting for.

## Implementation notes

- **Shell and service.** `macos/SupvanPrinter/main.swift` (AppKit, built with
  plain `swiftc` by `macos/build-app.sh`) starts
  `Contents/MacOS/supvan-printer-app` as a child with `SUPVAN_EVENTS=1` and
  talks to it over pipes: JSON events on stdout (`printers`, `link`,
  `status`, `job_started`, `job_done`, `job_held`, `job_failed`,
  `connect_failed`, `label_set`), commands on stdin (`connect`,
  `disconnect`, `set_label <width> <length> <gap>`,
  `set_darkness <0-100>`, `set_dither <mode>`,
  `print_pgm <density> <path>`, `quit`).
  EOF on stdin makes the service exit, so a crashed shell never leaves a
  daemon holding the printer. See `crates/supvan-app/src/events.rs`.
- **On-demand link.** `SUPVAN_ON_DEMAND=1` stops status polls from dialing
  (they use the link only while it is up, and otherwise report the queue as
  idle so it keeps accepting jobs); `SUPVAN_IDLE_DISCONNECT_SECS` hangs up
  an unused link. Both are plain environment settings and work on Linux too.
  The app also sets `SUPVAN_HOST=127.0.0.1` and `SUPVAN_MDNS=0`: the service
  is loopback-only and CUPS reaches it through the queue the app created.
- **Permission order.** The app raises the Bluetooth prompt itself (a
  `CBCentralManager`) and starts the service only once access is granted;
  IOBluetooth's first call would otherwise block on the pending prompt.
- **Run loops.** `supvan-proto/macos/rfcomm_shim.m` wraps IOBluetooth in a C
  byte-pipe API; `rfcomm_macos.rs` turns that into the same `ByteLink` the
  serial port implements, so framing and pacing are shared with Linux
  (`byte_link.rs`). The shim owns one thread for channel I/O, and the
  binaries hand their *main* thread to the system run loop
  (`supvan_proto::run_with_main_loop`): IOBluetooth's first use waits on a
  callback through the main dispatch queue, and a main thread parked in
  tokio would hang it.
- **Signing.** `build-app.sh` signs with the first code-signing identity in
  the keychain (or `SUPVAN_SIGN_IDENTITY`), falling back to ad-hoc. A real
  identity keeps the bundle's code requirement stable, so the Bluetooth
  grant survives rebuilds; an ad-hoc bundle is asked again after every
  change.
- **Status bit MSTA 0x10** (`ribbon_rw_error`, named after the Android app's
  register table) also means **label not seated** on a direct-thermal T50
  Pro: the LED goes red, the firmware accepts a job and drops `printing`
  without feeding. Pull the label's leading edge out past the cutter and
  close the cover; the bit clears at once. It is reported as `media-jam`,
  which holds the job instead of letting it silently "complete".

## Verified

On macOS 26 with a T50 Pro over Bluetooth: discovery from the paired list,
`supvan-cli probe` (status, firmware, protocol version, loaded roll), the
roll published as `media-ready` (`om_50x70mm_50x70mm`), and a 50×70 mm label
printed from a `-m everywhere` CUPS queue. Through the app: the Bluetooth
grant (kept across a rebuild signed with the same identity), queue creation,
Connect, Disconnect with no redial by status polls, the 90 s idle hang-up,
and a job sent while disconnected dialing, printing and completing.

A genuine Supvan roll does report a non-zero tag signature in `RETURN_MAT`
(seen on a 50×70 mm roll, catalogue code 5339) — the assumption
`record_is_ours` rests on, previously untested.

**Not yet confirmed:** USB on macOS; the held-job notification on real
hardware (that path is exercised by unit tests and the mock only); and
writing label settings from macOS (only a tagged roll was on hand, which is
refused by design — the write itself is the Linux-verified
`provision_material`). Please report what
`RUST_LOG=debug supvan-cli probe <target>` says for USB.
