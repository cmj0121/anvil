#!/usr/bin/env python3
# requires: python3 -m pip install cryptography; run: python3 tests/fixtures/update/make.py
"""Write deterministic update-verifier fixtures beside this file.

Three Ed25519 key pairs (A current, B next, C untrusted) are derived from
fixed seeds. Only the public keys are written. These are test keys and public
by design: anyone can rebuild their secret halves from the seeds below, so no
release may ever trust them. The tarball is signed in the
minisign prehash format (`ED`: Ed25519 over the BLAKE2b-512 of the file) and
once more as a legacy `Ed` signature over the file itself. The global
signature is Ed25519 over the 64 signature bytes followed by the trusted
comment. Secret keys never touch the disk.
"""

from __future__ import annotations

import base64
import gzip
import hashlib
import io
import tarfile
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
from cryptography.hazmat.primitives.serialization import Encoding, PublicFormat

HERE = Path(__file__).resolve().parent

TAG = "v9.9.9"
TRIPLE = "aarch64-apple-darwin"
ASSET = f"anvil-{TAG}-{TRIPLE}.tar.gz"
COMMENT = f"anvil {TAG} {TRIPLE}"
WRONG_COMMENT = f"anvil v9.9.8 {TRIPLE}"

# Fixed phrases, not raw key material. The seed is their SHA-256.
SEEDS = {
    "a": b"anvil update fixture key A current",
    "b": b"anvil update fixture key B next",
    "c": b"anvil update fixture key C untrusted",
}


def keypair(phrase: bytes):
    seed = hashlib.sha256(phrase).digest()
    private = Ed25519PrivateKey.from_private_bytes(seed)
    public = private.public_key().public_bytes(Encoding.Raw, PublicFormat.Raw)
    # minisign picks a key id at random; a fixed BLAKE2b-64 of the public
    # key keeps these fixtures deterministic.
    key_id = hashlib.blake2b(public, digest_size=8).digest()
    return private, public, key_id


def b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def public_key_text(public: bytes, key_id: bytes) -> str:
    """`untrusted comment:` then base64(`Ed` || key id || public key)."""
    blob = b"Ed" + key_id + public
    # minisign prints the key id as a little-endian u64.
    comment = "untrusted comment: minisign public key " + key_id[::-1].hex().upper()
    return f"{comment}\n{b64(blob)}\n"


def sign(private, key_id: bytes, payload: bytes, trusted: str, *, prehashed: bool) -> str:
    """A four-line `.minisig`. `prehashed` selects `ED` (BLAKE2b-512) or legacy `Ed`."""
    if "\n" in trusted or "\r" in trusted:
        raise SystemExit("trusted comment must be one line")
    if prehashed:
        message = hashlib.blake2b(payload, digest_size=64).digest()
        algorithm = b"ED"
    else:
        message = payload
        algorithm = b"Ed"
    signature = private.sign(message)
    # The global signature covers the raw signature bytes and the comment text,
    # not the `trusted comment:` prefix and not the algorithm or key id.
    global_sig = private.sign(signature + trusted.encode("utf-8"))
    body = algorithm + key_id + signature
    return (
        "untrusted comment: signature from minisign secret key\n"
        f"{b64(body)}\n"
        f"trusted comment: {trusted}\n"
        f"{b64(global_sig)}\n"
    )


def tarball_bytes() -> bytes:
    """`anvil-<tag>-<triple>/anvil`, gzip mtime 0, so two runs match byte for byte."""
    directory = f"anvil-{TAG}-{TRIPLE}"
    payload = b'#!/bin/sh\necho "anvil 9.9.9"\n'
    tar_buf = io.BytesIO()
    with tarfile.open(fileobj=tar_buf, mode="w", format=tarfile.USTAR_FORMAT) as tar:
        info = tarfile.TarInfo(directory)
        info.type = tarfile.DIRTYPE
        info.mode = 0o755
        info.mtime = 0
        info.uid = 0
        info.gid = 0
        info.uname = "root"
        info.gname = "root"
        tar.addfile(info)

        member = tarfile.TarInfo(f"{directory}/anvil")
        member.size = len(payload)
        member.mode = 0o755
        member.mtime = 0
        member.uid = 0
        member.gid = 0
        member.uname = "root"
        member.gname = "root"
        tar.addfile(member, io.BytesIO(payload))

    gz_buf = io.BytesIO()
    with gzip.GzipFile(filename="", mode="wb", fileobj=gz_buf, mtime=0, compresslevel=9) as gz:
        gz.write(tar_buf.getvalue())
    return gz_buf.getvalue()


def write_text(name: str, text: str) -> None:
    (HERE / name).write_text(text, encoding="ascii", newline="\n")


def main() -> None:
    keys = {name: keypair(phrase) for name, phrase in SEEDS.items()}
    for name, (_private, public, key_id) in keys.items():
        write_text(f"{name}.pub", public_key_text(public, key_id))

    blob = tarball_bytes()
    (HERE / ASSET).write_bytes(blob)
    digest = hashlib.sha256(blob).hexdigest()
    write_text(f"{ASSET}.sha256", f"{digest}  {ASSET}\n")

    priv_a, _pub_a, id_a = keys["a"]
    priv_b, _pub_b, id_b = keys["b"]
    priv_c, _pub_c, id_c = keys["c"]
    write_text(f"{ASSET}.minisig", sign(priv_a, id_a, blob, COMMENT, prehashed=True))
    write_text(f"{ASSET}.b.minisig", sign(priv_b, id_b, blob, COMMENT, prehashed=True))
    write_text(f"{ASSET}.c.minisig", sign(priv_c, id_c, blob, COMMENT, prehashed=True))
    write_text(
        f"{ASSET}.wrong.minisig",
        sign(priv_a, id_a, blob, WRONG_COMMENT, prehashed=True),
    )
    write_text(
        f"{ASSET}.legacy.minisig",
        sign(priv_a, id_a, blob, COMMENT, prehashed=False),
    )


if __name__ == "__main__":
    main()
