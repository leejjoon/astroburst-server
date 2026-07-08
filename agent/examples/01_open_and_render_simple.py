#!/usr/bin/env python3
"""
Example 01 — Open a FITS file and render it.

Usage:
    python 01_open_and_render.py /path/to/image.fits [slot_name]

The server must be running and reachable at http://localhost:8080.
Start it with:
    cargo run --no-default-features --features server,astrometry-net,asdf-full,vizier
"""
from __future__ import annotations

import asyncio
import sys
from pathlib import Path

from astroburst_client import AstroBurstClient, to_pillow
from astroburst_client.errors import AstroBurstError


async def main(fits_path: str, slot: str = "img") -> None:
    async with AstroBurstClient("http://localhost:8080") as client:
        # -- health check ---------------------------------------------------
        health = await client.health()
        session = await client.create_session()
        result = await session.open(fits_path, slot=slot)
        png, stf = await session.stf(slot)
        out.write_bytes(png)
