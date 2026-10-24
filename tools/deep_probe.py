#!/usr/bin/env python3
"""
deep_probe.py — проверка BLE-протокола кубика Deep с Mac.

Ставится один раз:   pip3 install bleak

Использование:
    python3 deep_probe.py scan                 # найти кубик и показать GATT
    python3 deep_probe.py watch                # слушать состояние (98658F39)
    python3 deep_probe.py play 1 --power max --side top --minutes 480
    python3 deep_probe.py pause
    python3 deep_probe.py stop

Ничего не пишет в OAD-сервис. Максимум, что делает, — отправляет 8 байт
в характеристику управления, ровно как это делает штатное приложение.
"""

import argparse
import asyncio
import struct
import sys

from bleak import BleakClient, BleakScanner

SVC_PROGRAM  = "98658f38-e4e3-4ac4-8e14-b1b7ff024353"
CHR_STATE    = "98658f39-e4e3-4ac4-8e14-b1b7ff024353"   # read + notify, 12 байт
CHR_CONTROL  = "98658f3a-e4e3-4ac4-8e14-b1b7ff024353"   # write, 8 байт
SVC_SETTINGS = "9b5bfb12-20d7-4730-aa0c-993ea99fd4cd"
CHR_SETTINGS = "9b5bfb13-20d7-4730-aa0c-993ea99fd4cd"
SVC_OAD      = "f000ffc0-0451-4000-b000-000000000000"

CMD   = {"play": 0x1A, "pause": 0x2B, "stop": 0x3C}
POWER = {"none": 0xA5, "min": 0x00, "medium": 0x7F, "max": 0xFF}
SIDE  = {"top": 1, "right": 2, "bottom": 3, "left": 4}
STATE = {0x00: "awaiting", 0xAA: "active", 0xBB: "paused"}


def control_packet(command, program_id=0, power="none", side=None,
                   seconds=None, led=None, vibration=None):
    """8 байт для характеристики 98658F3A."""
    b = bytearray(8)
    b[0] = CMD[command]
    b[1] = program_id & 0xFF
    b[2] = POWER[power]
    if program_id > 99:                       # медитация
        b[3] = (2 if led else 0) | (1 if vibration else 0)
    else:                                     # программа
        b[3] = SIDE[side] if side else 0
        if seconds:
            b[4:8] = struct.pack("<I", int(seconds))
    return bytes(b)


def parse_state(data: bytes):
    if len(data) != 12:
        return f"неизвестный пакет ({len(data)} байт): {data.hex(' ')}"
    a, bb = struct.unpack_from("<I", data, 4)[0], struct.unpack_from("<I", data, 8)[0]
    return (f"state={STATE.get(data[0], hex(data[0]))} "
            f"program={data[1]} power=0x{data[2]:02X} percent={data[3]}% "
            f"u32@4={a} u32@8={bb}  raw={data.hex(' ')}")


async def find_device(timeout=12.0):
    print(f"скан {timeout:.0f} с…")
    devices = await BleakScanner.discover(timeout=timeout, return_adv=True)
    hits = []
    for dev, adv in devices.values():
        uuids = {u.lower() for u in (adv.service_uuids or [])}
        if (SVC_PROGRAM in uuids or SVC_SETTINGS in uuids or SVC_OAD in uuids
                or (dev.name or "").lower().startswith("deep")):
            hits.append((dev, adv))
    if not hits:
        # запасной путь: подключиться к ближним безымянным устройствам и проверить GATT
        near = sorted((p for p in devices.values() if p[1].rssi > -80 and not p[0].name),
                      key=lambda p: -p[1].rssi)
        for dev, adv in near[:8]:
            print(f"  пробую {dev.address} rssi={adv.rssi}…", end=" ", flush=True)
            try:
                async with BleakClient(dev, timeout=6) as c:
                    uuids = {s.uuid.lower() for s in c.services}
                if SVC_PROGRAM in uuids:
                    print("ЭТО КУБИК")
                    return dev
                print("нет")
            except Exception as e:
                print(f"не подключился ({type(e).__name__})")
        print("кубик не найден в рекламе. Он мог не отдавать service UUID в advertising —")
        print("тогда включите его / выведите из сна и попробуйте ещё раз, либо")
        print("подключитесь по имени: BleakScanner.find_device_by_name('...').")
        for dev, adv in devices.values():
            print(f"  {dev.address}  rssi={adv.rssi:>4}  {dev.name!r}  {adv.service_uuids}")
        return None
    for dev, adv in hits:
        print(f"НАЙДЕН {dev.address}  rssi={adv.rssi}  name={dev.name!r}")
    return hits[0][0]


async def cmd_scan():
    dev = await find_device()
    if not dev:
        return
    async with BleakClient(dev) as c:
        print(f"\nподключено к {dev.address}\n")
        for svc in c.services:
            print(f"service {svc.uuid}  {svc.description}")
            for ch in svc.characteristics:
                props = ",".join(ch.properties)
                extra = ""
                if "read" in ch.properties:
                    try:
                        val = await c.read_gatt_char(ch)
                        try:
                            txt = val.decode("utf-8")
                            extra = f"  = {txt!r}"
                        except UnicodeDecodeError:
                            extra = f"  = {val.hex(' ')}"
                    except Exception as e:
                        extra = f"  <read failed: {e}>"
                print(f"    {ch.uuid}  [{props}]{extra}")


async def cmd_watch(seconds):
    dev = await find_device()
    if not dev:
        return
    async with BleakClient(dev) as c:
        cur = await c.read_gatt_char(CHR_STATE)
        print("read :", parse_state(cur))
        await c.start_notify(CHR_STATE, lambda _, d: print("notify:", parse_state(d)))
        print(f"слушаю {seconds} с… (Ctrl-C чтобы выйти)")
        await asyncio.sleep(seconds)
        await c.stop_notify(CHR_STATE)


async def cmd_send(packet, watch_after):
    dev = await find_device()
    if not dev:
        return
    async with BleakClient(dev) as c:
        await c.start_notify(CHR_STATE, lambda _, d: print("notify:", parse_state(d)))
        await c.start_notify(CHR_CONTROL, lambda _, d: print("ctrl-notify:", d.hex(" ")))
        print("before:", parse_state(await c.read_gatt_char(CHR_STATE)))
        print("write :", packet.hex(" "))
        await c.write_gatt_char(CHR_CONTROL, packet, response=True)
        await asyncio.sleep(watch_after)
        print("after :", parse_state(await c.read_gatt_char(CHR_STATE)))
        await c.stop_notify(CHR_STATE)


def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = p.add_subparsers(dest="cmd", required=True)

    sub.add_parser("scan")

    w = sub.add_parser("watch")
    w.add_argument("--seconds", type=int, default=120)

    pl = sub.add_parser("play")
    pl.add_argument("program_id", type=int)
    pl.add_argument("--power", choices=list(POWER), default="medium")
    pl.add_argument("--side", choices=list(SIDE), default="top")
    pl.add_argument("--minutes", type=int, default=60,
                    help="60..1440 (прошивка ждёт 3600..86400 секунд)")
    pl.add_argument("--led", action="store_true", help="только для id>99 (медитации)")
    pl.add_argument("--vibration", action="store_true", help="только для id>99")

    sub.add_parser("pause")
    sub.add_parser("stop")

    a = p.parse_args()

    if a.cmd == "scan":
        asyncio.run(cmd_scan())
    elif a.cmd == "watch":
        asyncio.run(cmd_watch(a.seconds))
    elif a.cmd == "play":
        if a.program_id <= 99 and not 60 <= a.minutes <= 1440:
            p.error("--minutes должно быть 60..1440: прошивка не принимает меньше часа")
        pkt = control_packet("play", a.program_id, a.power, a.side,
                             a.minutes * 60, a.led, a.vibration)
        asyncio.run(cmd_send(pkt, 10))
    elif a.cmd in ("pause", "stop"):
        asyncio.run(cmd_send(control_packet(a.cmd), 5))


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
