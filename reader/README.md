# Evolis Primacy card-slot RFID reader/writer

An ESP32 (M5Stack Atom Lite) sitting in the Primacy's encoder slot with a PN532
on I2C — 13.56 MHz, ISO14443A (MIFARE Classic, Type 2 / NTAG), reads and writes
NDEF — and its own HTTP API.

No custom component: the stock `pn532` component does the work and `web_server`
version 3 serves the entities over REST. Configuration:
`esphome/rfid-reader.yaml`. The consumer is the print spooler's card-UID read
(`printer/esplp_print`, `Printer.read_card_uid`), which parks the card at the
CONTACTLESS station first — see `EVOLIS-Internal-Ports.md`.

## Hardware

| Grove (HY2.0-4P) | Atom pin | PN532 |
|---|---|---|
| Black | GND | GND |
| Red | 5V | VCC |
| Yellow | G26 | SDA |
| White | G32 | SCL |

PN532 DIP switches: **SEL0 ON, SEL1 OFF** (I2C), address `0x24`. G26/G32 are the
board's own I2C defaults for the Grove port, which is the Atom's only connector
that carries power; G19, G21, G22, G23, G25 and G33 stay free.

- Power is the printer's internal USB-A port (downstream of the internal hub),
  which also makes the Atom's USB-UART bridge enumerate on the host PC —
  harmless. Budget ≈150 mA (Atom) + 100 mA (PN532). Never feed the Atom 5 V from
  two places at once.
- The antenna goes where the original encoder's antenna sat, at the CONTACTLESS
  station. The PN532's little coil is detuning-sensitive: a few mm of sheet metal
  in front of or behind it costs range, so give it clearance (and a ferrite
  backing if you have one) rather than seating it flat on the chassis.
- The chassis is steel: check the RSSI in the boot log before you close the
  printer. If the link is poor, a board with an external antenna is the fix.

## Firmware

```sh
cp esphome/secrets.yaml.example esphome/secrets.yaml   # wifi, api_key, web auth
openssl rand -base64 32                                # -> api_key

nix shell nixpkgs#esphome -c esphome run reader/esphome/rfid-reader.yaml   # first flash over USB
nix shell nixpkgs#esphome -c esphome compile reader/esphome/rfid-reader.yaml
nix shell nixpkgs#esphome -c esphome logs reader/esphome/rfid-reader.yaml  # over the network
```

Once flashed it is reachable as `evolis-rfid.local`; every later update goes
over OTA.

## HTTP API

`http://evolis-rfid.local/` (or the IP). Every request needs the `web_server`
credentials from `secrets.yaml` — the same auth guards this API, so the spooler
needs them:

```sh
curl -u admin:web-password http://evolis-rfid.local/text_sensor/rfid_uid_13mhz
```

The **entity name is the URL segment**, so the entities are named as slugs.
Renaming one in the YAML changes its URL.

### Entities

| Entity name | Request | Meaning |
|---|---|---|
| `rfid_uid_13mhz` | GET `/text_sensor/rfid_uid_13mhz` | UID of the card at the antenna, `04-A2-B3-C4`; empty when there is none |
| `rfid_card_13mhz` | GET `/binary_sensor/rfid_card_13mhz` | `ON` while a card is at the antenna |
| `rfid_ndef_13mhz` | GET `/text_sensor/rfid_ndef_13mhz` | that card's NDEF, one record per line as `T:Hello`, `U:https://…`, or `<mime-type>:<hex>` |
| `rfid_write_text` | POST `/text/rfid_write_text/set?value=…` | arm a write of one text record |
| `rfid_write_uri` | POST `/text/rfid_write_uri/set?value=…` | arm a write of one URI record |
| `rfid_format_tag` | POST `/button/rfid_format_tag/press` | arm an NDEF format |
| `rfid_clean_tag` | POST `/button/rfid_clean_tag/press` | arm an erase |

Reads answer with the entity's state:

```sh
$ curl -u admin:pw http://evolis-rfid.local/text_sensor/rfid_uid_13mhz
{"id":"text_sensor/rfid_uid_13mhz","state":"04-A2-B3-C4","value":"04-A2-B3-C4"}
$ curl -u admin:pw http://evolis-rfid.local/binary_sensor/rfid_card_13mhz
{"id":"binary_sensor/rfid_card_13mhz","state":"ON","value":true}
```

Reading a card is therefore two requests: presence, then the UID.

### Writing NDEF

The value travels in the query string, so it must be percent-encoded —
`--data-urlencode` does it:

```sh
curl -u admin:pw -X POST -G --data-urlencode 'value=Hello world' \
  http://evolis-rfid.local/text/rfid_write_text/set
curl -u admin:pw -X POST -G --data-urlencode 'value=https://example.com' \
  http://evolis-rfid.local/text/rfid_write_uri/set
curl -u admin:pw -X POST http://evolis-rfid.local/button/rfid_clean_tag/press
```

1. The POST **arms** the write and returns `200`.
2. Present a card: the PN532 formats and writes it.
3. Remove and re-tap the card, then `GET rfid_ndef_13mhz` to see what is on it.

Behaviour worth knowing:

- A card already lying on the antenna is **not** written — the PN532 only acts
  on a newly detected card. Take it off and present it again.
- An armed write is consumed by the next card presented, whoever presents it.
  It gives up after 30 s (`disarm_write`) so a forgotten request cannot rewrite
  a card later.
- The log says "NDEF write finished" after a successful format+write; a *format*
  failure logs an error and produces no finished line at all. Either way the tag
  is the only proof — a tag too small for the message fails without the API
  saying so.
- At most **4 records** per message (the component's limit). A text record is
  limited to 255 characters by the entity; NTAG213 holds only 144 bytes of NDEF
  in total, so a long text needs NTAG215/216.
- Setting the write value twice before presenting a card leaks the first
  message (a few hundred bytes) — the PN532 frees only what it has written.

## Limits

- **No ISO15693 / ICODE.** The PN532 component polls ISO14443A only.
- **NDEF is flattened to text**: one record per line, `type:payload`, no
  language code, newlines inside a payload are indistinguishable from a record
  separator.
- The UID is ESPHome's dash form (`04-A2-B3-C4`); the 12-digit `serial_number`
  is built from those four bytes.
- Browser-based clients are subject to the 2026.7.0 cross-origin rules
  (`allowed_origins`); `curl` and the spooler send no `Origin` header and are
  unaffected.
- **125 kHz (EM4100 / RDM6300) is deliberately not supported.** It would have
  been a second coil beside the antenna with no clear demand behind it; adding it
  back is a `uart:` block, an `rdm6300:` block on a free pin (G33) and a text
  sensor — the component's quirks (5 V TX needs a divider, `on_tag` fires per
  frame, the leading UID byte is dropped) are recorded in the
  `esphome-device-config` skill.

## Bring-up

`logger.level: DEBUG` logs every tag the reader sees, which is the fastest way to
tell "wiring is wrong" from "firmware is wrong":

- No tag at all, and the boot log shows `Found chip PN5xx`: the reader is fine
  and the card is not where the coil expects it.
- `update_interval` is the detection latency (500 ms here), so the presence
  entity going `ON` within a second of a presentation is the normal case.
- After mounting: if the range becomes marginal, suspect the metal around the
  coil, not the firmware. Measure a known card at a few distances, with and
  without the surrounding chassis, before touching the config.

## Feeding the print spooler

`serial_number` is a card's physical UID as four bytes printed as `%03d` each,
12 decimal digits (`004162179196` for `04-A2-B3-C4`) — see the UID rules in the
repo README and `MEDIAHUB-INTEGRATION.md`. So a UID read is:

```sh
curl -u admin:pw http://evolis-rfid.local/binary_sensor/rfid_card_13mhz   # wait for ON
curl -u admin:pw http://evolis-rfid.local/text_sensor/rfid_uid_13mhz      # then the UID
```

and the 12-digit value is built from the four `state` bytes. A card the reader
has not seen yields an empty `state`, which is the same "not known yet" the
spooler already handles as a missing UID.
