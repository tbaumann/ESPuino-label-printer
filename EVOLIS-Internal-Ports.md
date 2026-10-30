# Evolis Primacy — internal ports and encoder connectors

Printer inspected: **Evolis Primacy**, firmware 2607, S/N `10000558362`,
hostname `EVO_PR1_0558362`.

## Topology

The encoder modules are USB devices on an **internal USB hub**, not on any
printer-specific bus. The hub's upstream port is the printer's USB type-B, so
one USB cable from the PC drives both the printer and the encoders.

Primacy user guide, verbatim:

> If your encoder is supplied with an USB interface, it will either be connected
> to the printer's internal USB Hub, or equipped with a USB cable to connect to
> your computer directly. In the first case, your printer's USB cable is used to
> drive firstly the printer and secondly the encoder from the computer.

> These encoders are connected to your PC via a USB interface, which means
> encoding cannot be done with an Ethernet connection.

Rear connectors (same guide): **D - USB port (type A)**, **E - USB port (type
B)**, **F - Ethernet port**. D is a further downstream port of the same internal
hub — a device plugged into it is a device on the printer's internal hub, seen by
whoever hosts the hub (the PC over E).

The magnetic encoder is not on that hub; the printer drives it itself.

## Encoder modules

| Module | Interface | Appears as |
|---|---|---|
| Evolis Elyctis Dual (contact + contactless) | USB, "plugged to the USB port hub inside the Evolis printer" | 5 PC/SC readers: 1 contactless, 1 contact, 3 SIM/SAM |
| Evolis Elatec TWN4 Multitech Legic 42 | USB ("virtual serial port") | 3 PC/SC readers: 1 contactless, 2 SIM/SAM |
| Smart contact station ISO 7816-2 | USB | contact reader (PC/SC) |

Both vendor datasheets state PC/SC (CCID) support: Elyctis "supported by
PCSC-lite CCID driver" on Linux/Android; Elatec lists "Supported by PCSC-Lite
CCID driver" for Linux, marked *in preparation* on that sheet. The Elyctis
contactless reader's USB identity is **VID `0x1FFA`, PID
`0x0002`** (falls back to the Microsoft USB CCID driver when its own is absent);
its PC/SC driver file is `SCRPCSC.inf`.

Contactless options offered for the Primacy: ISO 14443A/B, ISO 15693, MIFARE,
DESFire, HID iCLASS, and on some parts UHF (EPC C1 Gen2 / ISO18000-6C).

## Connectors on the encoder board (Elyctis ELY007)

From *AN: Connectors Pinout and Smartcard Presence*, ELY007 v1.3 (Elyctis SDK,
`ELY Chipset based device/4 - Manuals/`).

### J2 — host link, JST B4B-PH-SM4-TB, right to left

| Pin | Signal |
|---|---|
| 1 | +5V |
| 2 | USB- |
| 3 | USB+ |
| 4 | GND |

This 4-pin USB link is the likely candidate for the pins visible at the reader
slot: plain USB plus 5 V, no UART or I2C host interface.

### J3 — JST B2B-PH-SM4-TB, right to left

| Pin | Signal |
|---|---|
| 1 | PRESENCE (smartcard presence switch) |
| 2 | GND |

Presence logic is implemented from ELY007 v2.x onwards.

### J4 — board-to-board, labelled "to ELY009", Hirose DF17(3.0)-20DS-0.5V(57), from the white dot clockwise

| Pin | Signal | Pin | Signal |
|---|---|---|---|
| 1 | USB- | 11 | GND |
| 2 | USB- | 12 | GND |
| 3 | NC | 13 | GND |
| 4 | USB+ | 14 | NC |
| 5 | USB+ | 15 | NC |
| 6 | NC | 16 | NC |
| 7 | NC | 17 | NC |
| 8 | 3V3 | 18 | 5V |
| 9 | 3V3 | 19 | 5V |
| 10 | 3V3 | 20 | 5V |

The antenna is a distinct board (Evolis Elyctis Dual datasheet: "a basic module
with a distinct antenna"), carrying its own USB pair and power rails.

### J6 — smartcard contacts, JST B8B-ZR-SM4-TF, top to bottom

| Pin | Signal |
|---|---|
| 1 | VCC smartcard |
| 2 | RESET smartcard |
| 3 | CLK smartcard |
| 4 | NC |
| 5 | NC |
| 6 | I/O smartcard |
| 7 | GND |
| 8 | NC |

## What the printer reports

`get_info()` exposes `hasSmartEnc`, `hasContactLessEnc`, `hasMagEnc`,
`hasScanner`. These are **option flags held by the printer's config**, not a live
enumeration of what is plugged in — on this unit all of them read False, i.e. no
encoder option is configured.

`evolis.PcscSession(connection)` drives the encoders through libevolis
(`src/c/evo-pcsc.cpp`: `getPCSCList`, `sendPCSCApdu`, `read_atr`,
`connect/disconnect`, `wait_card_present/absent`). libevolis does not link
libpcsclite, and over `socket://<ip>?type=evolis` `PcscSession.init()` answers
`ReturnCode.EUNSUPPORTED` (-5) — the encoders are only on the internal USB hub,
nothing about them is served on the printer's network channel (which exposes TCP
80 and 9100 only).

Card handling is the printer's job, not the reader's:

```python
co.set_card_pos(evolis.CardPos.CONTACTLESS)   # parks a fed card at the station
ps = evolis.PcscSession(co)                   # reader side (USB connection)
```

`CardPos.CONTACT` is the contact station, `CardPos.CONTACTLESS` the antenna.

## ESP32 bridge notes

- The module is a self-contained USB CCID reader. Hosted from an ESP32's USB
  port it presents as a smartcard reader; a UID read is the PC/SC pseudo-APDU
  `FF CA 00 00 00`.
- J2 pin 1 is +5V and pin 4 GND — fine to feed from the printer's hub or from
  the ESP32 side, but the module does not tolerate both at once.
- **The reader does not move a card.** The card must be positioned under the
  antenna by the printer itself (`set_card_pos`), so a UID read still needs a
  command path to the printer; the host cannot do it by talking to the reader.
- J3 pin 1 is a card-presence line — usable to know a card is at the station
  without polling the printer.

## The reader in the slot (the route in use)

The slot is not carrying one of those CCID modules: it holds our own ESP32 +
PN532 (`~/git/embassy-test/nfc-reader`), which polls the coil and serves what it
sees over the LAN — `GET /nfc/card`, whose `uid_decimal` is the card's UID, no
PC/SC anywhere in the path. The printer's side is unchanged and still the one
that matters: it moves the card to the station, and the spooler reads there
(`card-printer --reader`, see the README).

## Verifying on a host

Printer on USB (type-B), then:

```sh
lsusb -t                 # internal hub -> printer device(s) + reader(s) + type-A key
sudo dmesg | tail -30    # enumeration
lsblk                    # a USB stick in port D as /dev/sdX
pcsc_scan                # (pcsc-tools) the CCID readers, if a module is fitted
```

Over the network, with the SDK against `socket://<ip>?type=evolis`:
`/home/tilli/Downloads/evolis/evolis-pcsc-probe.py` prints the `has*Enc` flags,
the encoder list (`name`, `uid`) and per-encoder status, and with `--uid` parks a
card at the contactless station and reads its UID.

## Sources on this machine

- `~/Downloads/evolis/25-07-17 ELYCTIS SDK.zip` — Elyctis SDK: `ELY007` pinout AN,
  `ELY Chipset User Manual`, contact/contactless PC/SC drivers, firmware + DFU
  tooling, `Contact Reader.pdf`.
- `~/Downloads/EVOLIS ELATEC TWN4 LEGIC NFC.pdf` — Evolis Elatec datasheet
  (`KB-ENC1-1088-ENG-A4-A0`).
- `~/Downloads/evolis/evolis-sdk-python-wrapper/` — Python SDK 9.4.1
  (`PcscSession`, `PcscEncoder`, `PcscProtocol`, `PcscDisposition`, `PcscStatus`).
- Evolis Primacy user guide (encoders / internal hub / rear connectors section);
  "Evolis Elyctis Dual" datasheet (`KB-ENC1-129-ENG-A4-D0`).
