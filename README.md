<div align="center">

*Brought to you by*

  <a href="https://wizardsardine.com" target="_blank">
    <img src="ws_logo.png" width="400px" />
  </a>

</div>

# Bacca

Your hardware wallet Bitcoin companion.

**WARNING: this is alpha software. Only use for testing.**

A minimalistic software to update the firmware and the Bitcoin application of Ledger devices, and
the firmware of BitBox02 devices, without the vendor's software.

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

## Supported devices

- Ledger Nano S, Nano S Plus, Nano X, Stax, Flex and Nano Gen5;
- BitBox02 and BitBox02 Nova, Multi and Bitcoin-only editions, in firmware and bootloader mode (the
  discontinued BitBox01 is not supported).

Devices are only supported through USB. On Linux you need udev rules to access the devices as a
regular user (for a Ledger, see [Ledger's udev rules](https://github.com/LedgerHQ/udev-rules)).

This software can be used through a graphical interface (GUI), a command line interface (CLI), or
as Rust libraries (`ledger_manager` and `bitbox_manager`).

## GUI

Connect your device, unlock it and run:
```
cargo run --locked -p ledger_manager_gui
```

The GUI detects the connected device and shows its model, its firmware version and the latest
firmware available, with an "Update" button. For a Ledger it also shows whether it is genuine
("Check" button), and the installed and latest versions of the Bitcoin and Bitcoin Test apps, with
"Install" and "Update" buttons. For a BitBox it shows the edition (there are no apps to install on
a BitBox, see [BitBox](#bitbox)).

Before a firmware update, the GUI lists what the update implies and asks for confirmation. During
an operation it shows what to do on the device (allow the Ledger manager, unlock the device...) and
the codes to compare with the ones displayed by the device: the update identifier for a Ledger, the
pairing code and the firmware hash for a BitBox. Keep the device plugged in until the operation
completes.

If a Ledger firmware update was interrupted, run the GUI again: a Ledger in updater mode offers
"Update" to finish it, and a Ledger in bootloader mode offers "Repair".

## CLI

The CLI takes its command from an environment variable: `LEDGER_COMMAND` for a Ledger,
`BITBOX_COMMAND` for a BitBox. For instance:
```
LEDGER_COMMAND=checkfirm cargo run --locked -p ledger_manager_cli
LEDGER_COMMAND=updatefirm cargo run --locked -p ledger_manager_cli
LEDGER_TESTNET=1 LEDGER_COMMAND=installapp cargo run --locked -p ledger_manager_cli
BITBOX_COMMAND=updatefirm cargo run --locked -p ledger_manager_cli
```

### Ledger commands

- `getinfo`: show information about the device (model, firmware, mode) and the installed apps.
- `genuinecheck`: check the device is genuine.
- `installapp`, `updateapp`, `openapp`: install, update or open the Bitcoin app.
- `checkfirm`: show the current firmware and whether an update is available.
- `updatefirm`: update the firmware to the latest version, backing up the device settings before
  and restoring them after (see [below](#ledger-firmware-update)). Run it again to finish an
  interrupted update (device in updater mode).
- `repairfirm`: finish a firmware update interrupted while the device was updating its MCU or its
  bootloader (device stuck in bootloader mode), like Ledger Live's "repair your device".
- `restorebackup`: restore the settings from a backup made by `updatefirm`, for instance if the
  update was interrupted or the restoration failed.

Options:
- `LEDGER_TESTNET`: use the Bitcoin Test app instead of the Bitcoin app (`installapp`, `updateapp`,
  `openapp`).
- `LEDGER_BACKUP_DIR`: where `updatefirm` saves the backup of the device settings, and where
  `restorebackup` looks for it (by default the config directory, see below).
- `LEDGER_NO_BACKUP_FILE`: with `updatefirm`, only keep the backup in memory (it is lost if the
  update gets interrupted).
- `LEDGER_NO_RESTORE`: with `updatefirm`, don't back up nor restore anything (reinstall the Bitcoin
  app with `installapp` afterwards).
- `LEDGER_BACKUP_FILE`: the backup file to restore with `restorebackup` (by default the latest
  backup of this device model).
- `LEDGER_REPAIR_VERSION`: with `repairfirm`, force the first version to flash, like Ledger Live's
  repair options: `0.7` if the device says "MCU outdated" or "MCU not genuine", `0.9` if it tells
  to follow the repair or update instructions.

### BitBox commands

- `getinfo`: show the product, edition, versions and state of the device.
- `checkfirm`: show the latest firmware release for the device.
- `updatefirm`: update the device to the latest firmware release (this also installs a firmware on
  a device in bootloader mode without firmware).
- `reboot`: reboot a device in bootloader mode (this also clears its "start in bootloader mode"
  flag).

Options:
- `BITBOX_CONFIG_DIR`: where the pairing is remembered (by default the config directory, see
  below).
- `BITBOX_SHOW_HASH`: set to `1` (or `0`) to make the device show (or not) the firmware hash on
  every boot.
- `BITBOX_FORCE`: reinstall the firmware even if it is already installed.

## Ledger firmware update

A Ledger firmware update uninstalls all the apps, and may reset the language and the custom lock
screen picture of the device. Like Ledger Live, Bacca backs up before the update:
- which of the Bitcoin and Bitcoin Test apps are installed (the other apps are not reinstalled);
- the language of the device (Nano X, Nano S Plus, Stax, Flex, Nano Gen5);
- the custom lock screen picture (Stax, Flex, Nano Gen5). The device may ask you to approve its
  backup. If you refuse, or if the backup fails, the update continues without it.

After the update it restores, in this order: the language (by installing the language pack for the
new firmware, unless it was English), the lock screen picture, and the Bitcoin apps (their latest
version for the new firmware). You have to approve the language and the lock screen picture on the
device, and may have to allow the Ledger manager. Each part is restored independently, and a
report tells what was restored, skipped or failed and why.

The data stored inside the apps is **not** restored (neither by Ledger Live). In particular the
wallet policies registered in the Bitcoin app (multisig, [Liana](https://github.com/wizardsardine/liana)
wallets, ...) are lost: you may have to register your wallet again from your wallet software.

Before the update starts, the backup is saved to a file in the config directory (`~/.config/bacca`
on Linux, `~/Library/Application Support/bacca` on macOS, `%APPDATA%\bacca` on Windows), named
like `ledger-backup-<model>-<target id>-<date>.json`. If it can't be saved, the update is not
started: you can retry, or update keeping the backup in memory only. If the update gets
interrupted, the backup is restored when the update is resumed, and it can be restored at any time
with the `restorebackup` command of the CLI. The backup files are not deleted automatically.

During the update, the device must answer each command within 2 minutes (except when waiting for a
confirmation on the device), otherwise the update fails instead of hanging.

## BitBox

On the BitBox there is no separate Bitcoin app: the firmware edition (Multi or Bitcoin-only) *is*
the app, and it is fixed by the device's bootloader. Updating the firmware is updating the "Bitcoin
app". The edition of a device can't be changed; if a Multi edition is detected, the GUI recommends
the Bitcoin-only edition.

The latest signed firmware for the device is downloaded from the [official GitHub
releases](https://github.com/BitBoxSwiss/bitbox02-firmware/releases). Before flashing, the firmware
is validated (format, size, product and edition, no downgrade) and its hash is displayed, to
compare with the one published in the release notes and, if enabled, shown by the device at
startup. The signatures are verified by the device's bootloader. Like the BitBoxApp, Bacca first
installs and boots the required intermediate firmwares (v9.17.1, v9.26.2) when upgrading from an
old firmware.

Updating from firmware mode requires unlocking the device, confirming a pairing code the first
time, and confirming the reboot into the bootloader on the device. The pairing is remembered in
`bitbox.json` in the config directory.

Development bootloader: the intermediate firmware v9.26.2 upgrades the bootloader to v1.2.2, and it
refuses to replace a development bootloader (the device then halts on "Development bootloader";
unplug and replug it). The firmware releases after v9.26.2 are only signed for bootloaders v1.2.0
and later, so a device with an old development bootloader can't be updated past v9.26.2: the update
stops with an explanation. A development bootloader also waits for you to slide `<Continue>` on its
"DEV DEVICE" screen to boot the firmware.

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
