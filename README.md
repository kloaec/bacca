<div align="center">

*Brought to you by*

  <a href="https://wizardsardine.com" target="_blank">
    <img src="ws_logo.png" width="400px" />
  </a>

</div>

# Bacca

Your Ledger companion.

**WARNING: this is alpha software. Only use for testing.**

A minimalistic software to install and upgrade the Bitcoin application, and to update the firmware,
of Ledger devices.

![](./bacca_software_screenshot.png)

## Why?

Ledger makes great hardware. However their software is lacking.

The Ledger Nano S plus and X are secure, user-friendly and up to date with the latest Bitcoin
technologies. They are securely accessible to beginners, while letting their users benefit from
advancements and newer standards.

Ledger Live is a cluttered software to manage your device where development resources are allocated
toward scammy altcoins instead of making a decent Bitcoin wallet. The trajectory taken by Ledger
Live has become increasingly worrying to me and other users of Liana: will my beneficiaries at all
be able to navigate through the nudges toward Ponzi schemes and go through the unnecessary
complicated procedure of setting up their device to be used with a Bitcoin wallet?

This software offers a simple, straight-to-the-point, alternative.

The state of this project is nowhere near the point where it can stably replace Ledger Live for
non tech-savvy bitcoiners, yet. That said we hope to start pulling some of the functionalities into
[Liana](https://github.com/wizardsardine/liana).


## Usage

**This is a PoC. Use at your own risk.**

This software can be used:
1) Through a Graphical User Interface
2) Through a Command Line Interface
3) Through a Rust library for other projects to integrate some of the functionalities

### Supported devices

- Ledger Nano S
- Ledger Nano S Plus
- Ledger Nano X
- Ledger Stax
- Ledger Flex
- Ledger Nano Gen5

Devices are only supported through USB. On Linux you need the [Ledger udev
rules](https://github.com/LedgerHQ/udev-rules) to access the device as a regular user.

### GUI

The recommended way to use this software is through the GUI. Simply connect your Ledger (or
[BitBox](#bitbox)) device to the USB port, unlock it and run:
```
cargo run -p ledger_manager_gui
```

The GUI detects the connected device and shows:
- the device model and its current firmware version;
- the latest firmware available, with an "Update" button;
- for a Ledger, whether it is genuine ("Check" button), and the installed and latest versions of
  the Bitcoin and Bitcoin Test apps, with "Install" and "Update" buttons.

Before updating the firmware, the GUI lists what the update implies and asks for confirmation. The
progress of each operation is displayed at the bottom of the window, along with what to do on the
device: allow the Ledger manager, check that the identifier (Ledger) or the pairing code and the
firmware hash (BitBox) displayed on the device match the ones shown by the GUI, unlock the device,
etc. Keep the device plugged in until the operation completes.

A Ledger firmware update removes the apps from the device, and may reset its language and its
custom lock screen picture. Like Ledger Live, Bacca backs them up before the update and restores
them afterwards (see [Backup and restoration of the settings](#backup-and-restoration-of-the-settings)):
at the end of the update the GUI shows what was restored. You will have to approve the backup and
the restoration of the lock screen picture and of the language on the device. If the backup can't
be saved to a file, the update is not started: you can retry, or update without a backup file (the
backup is then only kept in memory). If a Ledger firmware update was interrupted, run the GUI
again: a Ledger in updater mode offers "Update" to finish it (the settings backed up before the
interrupted update are then restored), and a Ledger in bootloader mode offers "Repair".

We plan on releasing binaries in the future.

### CLI

Another way of using this is the CLI, which directly hooks up into the functionalities offered by
the Rust crate. The CLI will talk to a Ledger device connected by USB. The commands are communicated
using an environment variable, `LEDGER_COMMAND`. Another env var lets you switch to testnet (for
instance to install the test app), simply set `LEDGER_TESTNET` to any value.

For now those commands are implemented:
- `getinfo`: get information (such as the device model, the firmware version and the list of
  installed apps) for your device
- `genuinecheck`: check your Ledger device is genuine
- `installapp`: install the Bitcoin app on your device
- `updateapp`: update the Bitcoin app on your device
- `openapp`: open the Bitcoin app on your device
- `checkfirm`: show the current firmware version of your device and whether an update is available
- `updatefirm`: update the firmware of your device to the latest version, backing up the device
  settings before and restoring them after (see below)
- `repairfirm`: repair the firmware of a device stuck in bootloader mode, for instance because a
  firmware update was interrupted while updating the MCU or the bootloader (like Ledger Live's
  "repair your device"). Set `LEDGER_REPAIR_VERSION` to force the first version to flash, as
  Ledger Live's repair options do (`0.7` if the device says "MCU outdated" or "MCU not genuine",
  `0.9` if it tells to follow the repair or update instructions)
- `restorebackup`: restore the settings (apps, language, lock screen picture) from a backup made
  by `updatefirm`, for instance if the restoration failed or the update was interrupted. Set
  `LEDGER_BACKUP_FILE` to the backup file, by default the latest backup of the connected device
  model is used

Updating the firmware removes the applications installed on the device and may reset its language
and lock screen picture: `updatefirm` backs them up before the update and restores them afterwards,
and prints a report of what was restored (see [Backup and restoration of the
settings](#backup-and-restoration-of-the-settings)). Set `LEDGER_NO_RESTORE` to update without
backing up and restoring anything (reinstall the Bitcoin app with `installapp` afterwards). If
the backup can't be saved to a file, the update is not started: set `LEDGER_BACKUP_DIR` to save it
elsewhere, or `LEDGER_NO_BACKUP_FILE` to only keep it in memory. Keep the device connected during
the whole update, it may restart several times. If the update gets interrupted, run `updatefirm`
again (device in updater mode, the settings backed up before the interrupted update are then
restored) or `repairfirm` (device in bootloader mode) to complete it, and `restorebackup` if the
settings were not restored. During the update, the device must answer each command within 2
minutes (except when waiting for a confirmation on the device), otherwise the update fails with a
timeout rather than hanging. Other operations (installing apps, genuine check) have no such
timeout.

### Examples

#### Checking your Ledger is genuine

```
LEDGER_COMMAND=genuinecheck cargo run -p ledger_manager_cli
```
```
Querying Ledger's remote HSM to perform the genuine check. You might have to confirm the operation on your device.
Success. Your Ledger is genuine.
```

#### Updating the firmware of your Ledger

```
LEDGER_COMMAND=checkfirm cargo run -p ledger_manager_cli
LEDGER_COMMAND=updatefirm cargo run -p ledger_manager_cli
```

If the device is stuck in bootloader mode after an interrupted update:
```
LEDGER_COMMAND=repairfirm cargo run -p ledger_manager_cli
```

To restore the settings from a backup file:
```
LEDGER_COMMAND=restorebackup LEDGER_BACKUP_FILE=~/.config/bacca/ledger-backup-stax-33200004-20260924T110203Z.json cargo run -p ledger_manager_cli
```

#### Installing the Bitcoin Test app on your Ledger

```
LEDGER_TESTNET=1 LEDGER_COMMAND=installapp cargo run -p ledger_manager_cli
```
```
Querying installed applications from your Ledger. You might have to confirm on your device.
Querying Ledger's remote HSM to install the app. You might have to confirm the operation on your device.
Successfully installed the app.
```

### Backup and restoration of the settings

Like Ledger Live, before a Ledger firmware update Bacca backs up:
- the list of installed apps (all of them, not only the Bitcoin apps);
- the language of the device (Nano X, Nano S Plus, Stax, Flex, Nano Gen5);
- the custom lock screen picture (Stax, Flex, Nano Gen5). The device may ask you to approve its
  backup. If you refuse, or if the backup fails, the update continues without it.

After the update, it restores, in this order:
- the language, by installing the language pack for the new firmware (unless it was English). You
  have to approve it on the device;
- the lock screen picture. You have to approve it, and to confirm the picture, on the device;
- the apps, from the Ledger catalog for the new firmware (along with the apps they depend on). You
  may have to allow the Ledger manager on the device. The apps which are not available anymore
  for the new firmware are reported.

Each part is restored independently: a failure of one doesn't prevent the others, and the report
at the end tells what was restored, skipped or failed and why.

The data stored inside the apps is **not** restored (neither by Ledger Live). In particular the
wallet policies registered in the Bitcoin app (multisig, [Liana](https://github.com/wizardsardine/liana)
wallets, ...) are lost: you may have to register your wallet again from your wallet software.

Before the update starts, the backup is saved to a file in the config directory (`~/.config/bacca`
on Linux, `~/Library/Application Support/bacca` on macOS, `%APPDATA%\bacca` on Windows), named
like `ledger-backup-<model>-<target id>-<date>.json`. It contains the list of apps, the language
id and the lock screen picture (hex-encoded). If the update is interrupted, nothing is lost: the
backup is used when the update is resumed, and can be restored at any time with the
`restorebackup` command of the CLI. The backup files are not deleted automatically.

## BitBox

Bacca can also update the firmware of BitBox02 devices without the BitBoxApp, using the
`bitbox_manager` crate.

On the BitBox there is no separate Bitcoin application: the firmware edition (Multi or
Bitcoin-only) *is* the app, and is fixed by the device's bootloader. Updating the firmware is
updating the "Bitcoin app". The edition of a device cannot be changed.

Supported devices, in both firmware and bootloader mode:
- BitBox02 Multi and BitBox02 Bitcoin-only
- BitBox02 Nova Multi and BitBox02 Nova Bitcoin-only

The BitBox01 is discontinued and not supported.

The latest signed firmware for your device is downloaded from the [official GitHub
releases](https://github.com/BitBoxSwiss/bitbox02-firmware/releases). Before flashing, the firmware
file is validated (format, size, product/edition, no downgrade) and its hash is displayed so you
can compare it with the hash published in the release notes and, if enabled, shown by the device
at startup. The signatures are verified by the device's bootloader. Like the BitBoxApp, Bacca
first installs and boots the required intermediate firmwares (v9.17.1, v9.26.2) when upgrading
from an old firmware.

Updating from firmware mode requires unlocking the device, confirming a pairing code the first
time, and confirming the reboot into the bootloader on the device. The pairing is remembered in
`bitbox.json` in the config directory (`~/.config/bacca` on Linux, override with
`BITBOX_CONFIG_DIR`).

### GUI

The GUI detects a BitBox as well: it shows the product, the edition, the firmware version (or the
bootloader state) and the latest release, with an "Update" button. There is no app section for the
BitBox: if a Multi edition is detected, the GUI recommends the Bitcoin-only edition. During the
update, compare the pairing code and the firmware hash shown by the GUI with the ones displayed by
the device.

### CLI

The BitBox commands are passed through the `BITBOX_COMMAND` environment variable:
- `getinfo`: show the product, edition, versions and state of your device
- `checkfirm`: check the latest firmware release available for your device
- `updatefirm`: update your device to the latest firmware release (also installs a firmware on a
  device in bootloader mode without firmware)
- `flashfile`: flash the signed firmware file at `BITBOX_FIRMWARE_FILE`
- `hashfile`: show information and the hash of the signed firmware file at `BITBOX_FIRMWARE_FILE`

Optional: set `BITBOX_SHOW_HASH=1` (or `0`) to make the device show (or not) the firmware hash on
every boot, `BITBOX_FORCE` to reinstall the same firmware.

Development bootloader: the intermediate firmware v9.26.2 only upgrades the bootloader, and it refuses
to replace a development bootloader (the device then halts on "Development bootloader"; unplug and
replug it, it restarts in bootloader mode). The update detects this and installs the latest firmware
directly, or skip that step from the start with `BITBOX_SKIP_BOOTLOADER_UPGRADE=1`. A development
bootloader also waits for you to slide `<Continue>` on its "DEV DEVICE" screen to boot the firmware.

```
BITBOX_COMMAND=updatefirm cargo run -p ledger_manager_cli
```

## Dependencies and supply chain

This software talks to hardware wallets, so we try to keep its dependencies in check:
- `Cargo.lock` is committed and pins the exact version and checksum of every dependency. Build with
  `--locked` (e.g. `cargo run --locked -p ledger_manager_gui`) so cargo fails instead of silently
  picking other versions.
- All dependencies come from crates.io, no git dependency (enforced by `cargo deny`, see
  `deny.toml`).
- `cargo audit` (RustSec advisory database, configured in `.cargo/audit.toml`) and `cargo deny check`
  (advisories, sources, licenses) run in CI, next to `--locked` builds and tests.
- When updating dependencies, we only lock versions published at least 14 days ago, except for
  security fixes. Most malicious releases are detected and yanked within days.
- The few advisories ignored are for unmaintained or unsound crates only used by the GUI toolkit
  (iced 0.12), not by the libraries nor the CLI. They go away with an upgrade of iced.

## Future

We are looking into people to help test this and confirm it works in as many scenarii as possible.

Contributions welcome! If you are interested, get in touch on the [Liana
Discord](https://discord.gg/QJUp67zSN4).

NOTE: i am not interested in supporting altcoins. If you want to add support for one, feel free to
fork the project.
