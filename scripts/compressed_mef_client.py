#!/usr/bin/env python3
"""Fetch a RICE_1-compressed MEF FITS file from astroburst-server and
reconstruct the original (uncompressed) multi-extension file from it.

Usage:
    python compressed_mef_client.py <source_path_on_server> <output.fits> [quantize_level]

The server can be addressed directly (http://...) or through an
app-managed SSH tunnel (ssh://host or an ~/.ssh/config alias via
BASE_URL="ssh://olaf1"), which spawns `astroburst-server connect --json`
under the hood -- see remote_server() and issue #2.

Requires: requests, astropy (and the astroburst-server binary on PATH
for ssh:// targets)
"""

import contextlib
import json
import subprocess
import sys

import requests
from astropy.io import fits


@contextlib.contextmanager
def remote_server(ssh_target: str, binary: str = "astroburst-server"):
    """Yield a local base URL for a remote server reached over SSH.

    Spawns `astroburst-server connect <target> --json`, which picks a free
    local port, establishes the tunnel (reusing your ~/.ssh/config, keys,
    and agent), health-checks the remote server, and auto-reconnects on
    drops. The tunnel lives for the duration of the `with` block.
    """
    proc = subprocess.Popen(
        [binary, "connect", ssh_target, "--json"],
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        line = proc.stdout.readline()
        if not line:
            raise RuntimeError(f"connect exited without output (target {ssh_target!r})")
        info = json.loads(line)
        yield info["url"]
    finally:
        proc.terminate()
        proc.wait(timeout=10)


def fetch_compressed_fits(
    base_url: str,
    source_path: str,
    output_path: str,
    quantize_level: float | None = None,
    session_id: str | None = None,
    image_ref: str | None = None,
) -> str:
    """Fetch a compressed FITS file from the server and save it to `output_path`.

    If `session_id` is omitted, a new session is created and `source_path`
    (a path on the *server's* filesystem) is opened into it. Pass an
    existing `session_id` (and optionally `image_ref`) to reuse a session
    you already opened yourself.

    `quantize_level` controls the lossy float-quantization step (smaller =
    coarser/smaller file); omit to use the server's default (16.0).
    """
    if session_id is None:
        session_id = requests.post(f"{base_url}/v2/sessions").json()["session_id"]
        requests.post(
            f"{base_url}/v2/sessions/{session_id}/open",
            json={"path": source_path},
        ).raise_for_status()

    body = {}
    if image_ref is not None:
        body["image_ref"] = image_ref
    if quantize_level is not None:
        body["quantize_level"] = quantize_level

    resp = requests.post(
        f"{base_url}/v2/sessions/{session_id}/export/compressed", json=body
    )
    resp.raise_for_status()

    with open(output_path, "wb") as f:
        f.write(resp.content)

    return output_path


def reconstruct_mef(compressed_path: str, output_path: str) -> str:
    """Reconstruct an uncompressed multi-extension FITS file from a
    RICE_1-compressed one (as produced by /export/compressed).

    Every `CompImageHDU` is decompressed and rewritten as a plain
    `ImageHDU`/`PrimaryHDU` (accessing `.data` triggers the decompression;
    `.header` is astropy's already-reconstructed image-oriented header --
    EXTNAME/WCS/BUNIT/etc -- not the raw BINTABLE header). Anything else
    (the dataless primary, passthrough BinTables like an auxiliary
    wavelength-calibration table) is copied through unchanged.
    """
    with fits.open(compressed_path) as hdul:
        new_hdus = []
        for i, hdu in enumerate(hdul):
            if isinstance(hdu, fits.CompImageHDU):
                data = hdu.data
                header = hdu.header
                if i == 0:
                    new_hdus.append(fits.PrimaryHDU(data=data, header=header))
                else:
                    new_hdus.append(fits.ImageHDU(data=data, header=header))
            else:
                new_hdus.append(hdu.copy())

        fits.HDUList(new_hdus).writeto(output_path, overwrite=True)

    return output_path


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} <source_path_on_server> <output.fits> [quantize_level]")
        sys.exit(1)

    source_path = sys.argv[1]
    output_path = sys.argv[2]
    quantize_level = float(sys.argv[3]) if len(sys.argv) > 3 else None

    import os

    # http://host:port for a directly reachable server, or ssh://target /
    # ssh-config alias for an app-managed tunnel.
    base_url = os.environ.get("BASE_URL", "http://127.0.0.1:8080")

    def run(url: str):
        compressed_path = output_path + ".compressed"
        fetch_compressed_fits(url, source_path, compressed_path, quantize_level)
        print(f"Downloaded compressed file: {compressed_path}")
        reconstruct_mef(compressed_path, output_path)
        print(f"Reconstructed uncompressed file: {output_path}")

    if base_url.startswith("ssh://"):
        with remote_server(base_url) as url:
            run(url)
    else:
        run(base_url)
