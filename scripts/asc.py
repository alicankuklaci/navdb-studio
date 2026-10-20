#!/usr/bin/env python3
"""App Store Connect setup for the Mac App Store edition (see docs/APPSTORE.md).

    scripts/asc.py signing        bundle ID, installer certificate, provisioning profile
    scripts/asc.py subscriptions  Pro subscription group, products, prices, free trial

Needs ASC_KEY_ID, ASC_ISSUER_ID and the AuthKey_<id>.p8 file (ASC_KEY_FILE,
default ~/.config/appstoreconnect/AuthKey_<id>.p8). Requires PyJWT and
cryptography. Every step is idempotent: existing objects are reused.
"""

import base64
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import jwt

BUNDLE_ID = "dev.navdb.studio"
APP_NAME = "NavDB Studio"
ROOT = Path(__file__).resolve().parent.parent
PROFILE_OUT = ROOT / "packaging/macos/appstore.provisionprofile"
SECRETS = Path.home() / ".config/appstoreconnect/navdb"

# (product id, reference name, period, USD price) -- docs/APPSTORE.md explains the choice
PRODUCTS = [
    ("dev.navdb.studio.pro.monthly", "Pro Monthly", "ONE_MONTH", "6.99"),
    ("dev.navdb.studio.pro.yearly", "Pro Yearly", "ONE_YEAR", "49.99"),
]
TRIAL = "TWO_WEEKS"

KEY_ID = os.environ.get("ASC_KEY_ID", "")
ISSUER = os.environ.get("ASC_ISSUER_ID", "")
KEY_FILE = Path(os.environ.get("ASC_KEY_FILE", Path.home() / f".config/appstoreconnect/AuthKey_{KEY_ID}.p8"))


def token():
    now = int(time.time())
    claims = {"iss": ISSUER, "iat": now, "exp": now + 900, "aud": "appstoreconnect-v1"}
    return jwt.encode(claims, KEY_FILE.read_text(), algorithm="ES256", headers={"kid": KEY_ID})


def api(method, path, body=None):
    url = path if path.startswith("http") else "https://api.appstoreconnect.apple.com" + path
    req = urllib.request.Request(url, method=method, data=json.dumps(body).encode() if body else None)
    req.add_header("Authorization", "Bearer " + token())
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req) as r:
            data = r.read()
            return json.loads(data) if data else {}
    except urllib.error.HTTPError as e:
        detail = e.read().decode(errors="replace")
        raise SystemExit(f"{method} {path}: HTTP {e.code}\n{detail}")


def get_all(path):
    out, url = [], path
    while url:
        r = api("GET", url)
        out += r.get("data", [])
        url = r.get("links", {}).get("next")
    return out


def rel(kind, id_):
    return {"data": {"type": kind, "id": id_}}


def team_id():
    out = subprocess.run(["security", "find-identity", "-v", "-p", "codesigning"], capture_output=True, text=True).stdout
    for line in out.splitlines():
        if '"Apple Distribution:' in line:
            return line.rsplit("(", 1)[1].split(")")[0]
    raise SystemExit("no Apple Distribution identity in the keychain")


def bundle_id():
    found = get_all(f"/v1/bundleIds?filter[identifier]={BUNDLE_ID}")
    found = [b for b in found if b["attributes"]["identifier"] == BUNDLE_ID]
    if found:
        print("bundle ID exists:", BUNDLE_ID)
        return found[0]["id"]
    r = api("POST", "/v1/bundleIds", {"data": {"type": "bundleIds", "attributes": {"identifier": BUNDLE_ID, "name": APP_NAME, "platform": "MAC_OS"}}})
    print("bundle ID created:", BUNDLE_ID)
    return r["data"]["id"]


def keychain_has(prefix):
    out = subprocess.run(["security", "find-identity", "-v"], capture_output=True, text=True).stdout
    return any(f'"{prefix}' in l and f"({team_id()})" in l for l in out.splitlines())


def installer_certificate():
    """Create a Mac Installer Distribution certificate and import it with its key."""
    if keychain_has("3rd Party Mac Developer Installer") or keychain_has("Mac Installer Distribution"):
        print("installer certificate already in the keychain")
        return
    SECRETS.mkdir(parents=True, exist_ok=True)
    os.chmod(SECRETS, 0o700)
    key, csr = SECRETS / "installer.key", SECRETS / "installer.csr"
    subprocess.run(["openssl", "req", "-new", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", csr,
                    "-subj", f"/CN={APP_NAME} Installer/C=TR"], check=True, capture_output=True)
    os.chmod(key, 0o600)
    r = api("POST", "/v1/certificates", {"data": {"type": "certificates", "attributes": {
        "certificateType": "MAC_INSTALLER_DISTRIBUTION", "csrContent": csr.read_text()}}})
    cer = SECRETS / "installer.cer"
    cer.write_bytes(base64.b64decode(r["data"]["attributes"]["certificateContent"]))
    for f in (key, cer):
        subprocess.run(["security", "import", f, "-k", str(Path.home() / "Library/Keychains/login.keychain-db"),
                        "-T", "/usr/bin/productbuild", "-T", "/usr/bin/codesign"], check=True, capture_output=True)
    print("installer certificate created and imported")


def distribution_certificate_id():
    """The App Store Connect id of the Apple Distribution certificate in the keychain."""
    out = subprocess.run(["security", "find-certificate", "-a", "-c", "Apple Distribution", "-Z"], capture_output=True, text=True).stdout
    local = {l.split(":")[1].strip().upper() for l in out.splitlines() if l.startswith("SHA-1 hash:")}
    import hashlib
    for c in get_all("/v1/certificates?filter[certificateType]=DISTRIBUTION&limit=200"):
        content = base64.b64decode(c["attributes"]["certificateContent"])
        if hashlib.sha1(content).hexdigest().upper() in local:
            return c["id"]
    raise SystemExit("the keychain's Apple Distribution certificate was not found in this team")


def profile(bundle):
    name = f"{APP_NAME} Mac App Store"
    for p in get_all("/v1/profiles?filter[profileType]=MAC_APP_STORE&limit=200"):
        if p["attributes"]["name"] == name and p["attributes"]["profileState"] == "ACTIVE":
            content = p["attributes"]["profileContent"]
            print("provisioning profile exists:", name)
            break
    else:
        r = api("POST", "/v1/profiles", {"data": {"type": "profiles", "attributes": {"name": name, "profileType": "MAC_APP_STORE"},
                                                  "relationships": {"bundleId": rel("bundleIds", bundle),
                                                                    "certificates": {"data": [{"type": "certificates", "id": distribution_certificate_id()}]}}}})
        content = r["data"]["attributes"]["profileContent"]
        print("provisioning profile created:", name)
    PROFILE_OUT.write_bytes(base64.b64decode(content))
    print("saved", PROFILE_OUT.relative_to(ROOT))


def app_id():
    apps = get_all(f"/v1/apps?filter[bundleId]={BUNDLE_ID}")
    if not apps:
        raise SystemExit(f"no App Store Connect app for {BUNDLE_ID} yet: create it under Apps → + → New App")
    return apps[0]["id"]


def subscriptions():
    app = app_id()
    groups = get_all(f"/v1/apps/{app}/subscriptionGroups")
    if groups:
        group = groups[0]["id"]
    else:
        group = api("POST", "/v1/subscriptionGroups", {"data": {"type": "subscriptionGroups", "attributes": {"referenceName": "NavDB Studio Pro"},
                                                              "relationships": {"app": rel("apps", app)}}})["data"]["id"]
        api("POST", "/v1/subscriptionGroupLocalizations", {"data": {"type": "subscriptionGroupLocalizations",
                                                                   "attributes": {"locale": "en-US", "name": "NavDB Studio Pro"},
                                                                   "relationships": {"subscriptionGroup": rel("subscriptionGroups", group)}}})
        print("subscription group created")
    existing = {s["attributes"]["productId"]: s["id"] for s in get_all(f"/v1/subscriptionGroups/{group}/subscriptions")}
    for level, (pid, ref, period, usd) in enumerate(PRODUCTS, start=1):
        if pid in existing:
            print("subscription exists:", pid)
            continue
        sub = api("POST", "/v1/subscriptions", {"data": {"type": "subscriptions",
                                                        "attributes": {"name": ref, "productId": pid, "subscriptionPeriod": period,
                                                                       "familySharable": False, "groupLevel": level,
                                                                       "reviewNote": "Open Help → NavDB Studio Pro… to see the subscription window."},
                                                        "relationships": {"group": rel("subscriptionGroups", group)}}})["data"]["id"]
        label = "Monthly" if period == "ONE_MONTH" else "Yearly"
        api("POST", "/v1/subscriptionLocalizations", {"data": {"type": "subscriptionLocalizations",
                                                              "attributes": {"locale": "en-US", "name": f"NavDB Studio Pro {label}",
                                                                             "description": "Unlimited connections, data sync and automation"},
                                                              "relationships": {"subscription": rel("subscriptions", sub)}}})
        set_prices(sub, usd)
        api("POST", "/v1/subscriptionIntroductoryOffers", {"data": {"type": "subscriptionIntroductoryOffers",
                                                                   "attributes": {"duration": TRIAL, "offerMode": "FREE_TRIAL", "numberOfPeriods": 1},
                                                                   "relationships": {"subscription": rel("subscriptions", sub)}}})
        print(f"subscription created: {pid} ${usd} with a free trial")


def set_prices(sub, usd):
    """US price plus Apple's equalized price in every other storefront."""
    points = get_all(f"/v1/subscriptions/{sub}/pricePoints?filter[territory]=USA&limit=200")
    point = next(p for p in points if p["attributes"]["customerPrice"] == usd)
    targets = [point["id"]] + [p["id"] for p in get_all(f"/v1/subscriptionPricePoints/{point['id']}/equalizations?limit=200")]
    for pp in targets:
        api("POST", "/v1/subscriptionPrices", {"data": {"type": "subscriptionPrices", "attributes": {"preserveCurrentPrice": False},
                                                       "relationships": {"subscription": rel("subscriptions", sub),
                                                                         "subscriptionPricePoint": rel("subscriptionPricePoints", pp)}}})


def main():
    if not (KEY_ID and ISSUER and KEY_FILE.exists()):
        raise SystemExit("set ASC_KEY_ID and ASC_ISSUER_ID (and ASC_KEY_FILE if the .p8 is elsewhere)")
    cmd = sys.argv[1] if len(sys.argv) > 1 else ""
    if cmd == "signing":
        b = bundle_id()
        installer_certificate()
        profile(b)
    elif cmd == "subscriptions":
        subscriptions()
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
