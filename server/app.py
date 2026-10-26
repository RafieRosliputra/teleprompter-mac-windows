#!/usr/bin/env python3
"""JohorStudio Teleprompter — script server (tele.johorstudio.com).

Python standard library only (no pip install), so it runs straight from the
official `python:3.13-alpine` image with this folder mounted.

Public:
  GET  /                      submit / open page
  GET  /s/<code>              open a script by its 6-digit code (view + edit)
  POST /api/scripts           {title,name,email,content} -> {code}  (code is emailed)
  GET  /api/scripts/<code>    -> script JSON
  PUT  /api/scripts/<code>    {title?,content} -> updated script JSON
  POST /api/forgot            {email} -> emails every code for that address
Admin (header  Authorization: Bearer <ADMIN_TOKEN>):
  GET    /api/admin/scripts         list (no content)
  DELETE /api/admin/scripts/<code>  delete
Web admin (behind Cloudflare Access; the Access JWT is verified here too):
  GET    /admin                     admin page (list + delete)
  GET    /admin/api/scripts         list
  DELETE /admin/api/scripts/<code>  delete
"""
import base64
import hashlib
import json
import urllib.request
import os
import re
import secrets
import smtplib
import sqlite3
import ssl
import threading
import time
from email.message import EmailMessage
from email.utils import formatdate, make_msgid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HERE = os.path.dirname(os.path.abspath(__file__))
DB_PATH = os.environ.get("DB_PATH", "/data/tele.db")
PORT = int(os.environ.get("PORT", "8000"))
ADMIN_TOKEN = os.environ.get("ADMIN_TOKEN", "")
PUBLIC_URL = os.environ.get("PUBLIC_URL", "https://tele.johorstudio.com").rstrip("/")
SMTP_HOST = os.environ.get("SMTP_HOST", "")
SMTP_PORT = int(os.environ.get("SMTP_PORT", "587"))
SMTP_USER = os.environ.get("SMTP_USER", "")
SMTP_PASSWORD = os.environ.get("SMTP_PASSWORD", "")
SMTP_STARTTLS = os.environ.get("SMTP_STARTTLS", "true").lower() == "true"
SMTP_VERIFY = os.environ.get("SMTP_VERIFY", "false").lower() == "true"
EMAIL_FROM = os.environ.get("EMAIL_FROM", "JohorStudio <noreply@johorstudio.com>")
REPLY_TO = os.environ.get("REPLY_TO", "")
NOTIFY_EMAIL = os.environ.get("NOTIFY_EMAIL", "")  # owner gets a copy of each new script
# Cloudflare Access (web admin at /admin)
CF_ACCESS_TEAM = os.environ.get("CF_ACCESS_TEAM", "")  # e.g. "johorstudio"
CF_ACCESS_AUD = os.environ.get("CF_ACCESS_AUD", "")
ADMIN_EMAILS = {
    e.strip().lower() for e in os.environ.get("ADMIN_EMAILS", "").split(",") if e.strip()
}
MAX_CONTENT = 300_000
CODE_RE = re.compile(r"^\d{6}$")
EMAIL_RE = re.compile(r"^[^@\s]+@[^@\s]+\.[^@\s]+$")

_db_lock = threading.Lock()


def db():
    conn = sqlite3.connect(DB_PATH, timeout=10)
    conn.row_factory = sqlite3.Row
    return conn


def init_db():
    os.makedirs(os.path.dirname(DB_PATH) or ".", exist_ok=True)
    with db() as c:
        c.execute(
            """CREATE TABLE IF NOT EXISTS scripts(
                code TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                name TEXT NOT NULL,
                email TEXT NOT NULL,
                content TEXT NOT NULL,
                created INTEGER NOT NULL,
                updated INTEGER NOT NULL)"""
        )
        c.execute("CREATE INDEX IF NOT EXISTS idx_email ON scripts(email)")


# ---------------------------------------------------------------- rate limit
_hits = {}
_hits_lock = threading.Lock()


def limited(key, limit, window):
    """True if `key` exceeded `limit` events in `window` seconds."""
    now = time.time()
    with _hits_lock:
        arr = [t for t in _hits.get(key, []) if now - t < window]
        arr.append(now)
        _hits[key] = arr
        if len(_hits) > 5000:
            for k in list(_hits)[:1000]:
                _hits.pop(k, None)
        return len(arr) > limit


# ---------------------------------------------------------------- email
def send_mail(to, subject, body):
    if not SMTP_HOST:
        print(f"[mail disabled] to={to} subject={subject}", flush=True)
        return False
    msg = EmailMessage()
    msg["From"] = EMAIL_FROM
    msg["To"] = to
    msg["Subject"] = subject
    msg["Date"] = formatdate(localtime=True)
    msg["Message-ID"] = make_msgid(domain="johorstudio.com")
    if REPLY_TO:
        msg["Reply-To"] = REPLY_TO
    msg.set_content(body)
    try:
        ctx = ssl.create_default_context()
        if not SMTP_VERIFY:
            ctx.check_hostname = False
            ctx.verify_mode = ssl.CERT_NONE
        with smtplib.SMTP(SMTP_HOST, SMTP_PORT, timeout=20) as s:
            if SMTP_STARTTLS:
                s.starttls(context=ctx)
            if SMTP_USER:
                s.login(SMTP_USER, SMTP_PASSWORD)
            s.send_message(msg)
        print(f"[mail sent] to={to} subject={subject}", flush=True)
        return True
    except Exception as e:  # never fail the request because of email
        print(f"[mail error] to={to}: {e!r}", flush=True)
        return False


def mail_async(*args):
    threading.Thread(target=send_mail, args=args, daemon=True).start()


def code_email(name, title, code):
    return (
        f"Salam {name},\n\n"
        f"Terima kasih kerana menghantar skrip teleprompter kepada JohorStudio.\n\n"
        f"Tajuk : {title}\n"
        f"Kod   : {code}\n\n"
        f"Simpan kod 6 digit ini. Anda boleh buka dan edit skrip ini bila-bila masa sebelum acara:\n"
        f"{PUBLIC_URL}/s/{code}\n\n"
        f"Berikan kod ini kepada krew JohorStudio pada hari acara.\n\n"
        f"— JohorStudio.com\nhttps://johorstudio.com\n"
    )


# ---------------------------------------------------------------- data
def new_code(c):
    for _ in range(50):
        code = f"{secrets.randbelow(900000) + 100000}"
        if not c.execute("SELECT 1 FROM scripts WHERE code=?", (code,)).fetchone():
            return code
    raise RuntimeError("no free code")


def row_public(r):
    return {
        "code": r["code"],
        "title": r["title"],
        "name": r["name"],
        "content": r["content"],
        "created": r["created"],
        "updated": r["updated"],
    }


def clean(s, n):
    return (s or "").replace("\r\n", "\n").replace("\r", "\n").strip()[:n]


# ---------------------------------------------------------------- Cloudflare Access
# Verifies the Cf-Access-Jwt-Assertion header (RS256) against the team's public
# keys, so a request that bypasses Cloudflare (e.g. straight to :8130 on the LAN)
# can't fake being the admin.
_keys = {"at": 0.0, "keys": {}}
_keys_lock = threading.Lock()
_SHA256_PREFIX = bytes.fromhex("3031300d060960864801650304020105000420")


def _b64u(s):
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def _access_keys(force=False):
    with _keys_lock:
        if force or time.time() - _keys["at"] > 3600 or not _keys["keys"]:
            url = f"https://{CF_ACCESS_TEAM}.cloudflareaccess.com/cdn-cgi/access/certs"
            with urllib.request.urlopen(url, timeout=10) as r:
                data = json.load(r)
            _keys["keys"] = {
                k["kid"]: (int.from_bytes(_b64u(k["n"]), "big"), int.from_bytes(_b64u(k["e"]), "big"))
                for k in data.get("keys", [])
                if k.get("kty") == "RSA"
            }
            _keys["at"] = time.time()
        return _keys["keys"]


def access_email(token):
    """Email of a valid Cloudflare Access user, else None."""
    if not (token and CF_ACCESS_TEAM and CF_ACCESS_AUD):
        return None
    try:
        h64, p64, s64 = token.split(".")
        hdr = json.loads(_b64u(h64))
        if hdr.get("alg") != "RS256":
            return None
        keys = _access_keys()
        if hdr.get("kid") not in keys:
            keys = _access_keys(force=True)
        n, e = keys[hdr["kid"]]
        k = (n.bit_length() + 7) // 8
        em = pow(int.from_bytes(_b64u(s64), "big"), e, n).to_bytes(k, "big")
        digest = hashlib.sha256(f"{h64}.{p64}".encode()).digest()
        want = b"\x00\x01" + b"\xff" * (k - 3 - len(_SHA256_PREFIX) - 32) + b"\x00" + _SHA256_PREFIX + digest
        if not secrets.compare_digest(em, want):
            return None
        claims = json.loads(_b64u(p64))
        aud = claims.get("aud")
        aud = aud if isinstance(aud, list) else [aud]
        if CF_ACCESS_AUD not in aud:
            return None
        if claims.get("iss") != f"https://{CF_ACCESS_TEAM}.cloudflareaccess.com":
            return None
        if float(claims.get("exp", 0)) < time.time():
            return None
        email = (claims.get("email") or "").lower()
        return email if email and (not ADMIN_EMAILS or email in ADMIN_EMAILS) else None
    except Exception as ex:
        print(f"[access] verify failed: {ex!r}", flush=True)
        return None


# ---------------------------------------------------------------- phone-remote relay
# The Mac app connects OUT to /relay/mac/<session>; phones open /r/<session>
# (page) and connect to /relay/phone/<session>. Mac messages (layout/state)
# fan out to the phones, phone commands go to the Mac. Minimal RFC 6455.
SID_RE = re.compile(r"^[a-z0-9]{16,64}$")
_WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_PHONES = 10
_relay = {}  # sid -> {"mac": WS|None, "phones": set, "layout": str|None, "state": str|None}
_relay_lock = threading.Lock()


class WS:
    def __init__(self, handler):
        self.rfile = handler.rfile
        self.sock = handler.connection
        self.lock = threading.Lock()
        self.alive = True

    def send(self, text):
        data = text.encode("utf-8")
        n = len(data)
        if n < 126:
            head = bytes([0x81, n])
        elif n < 65536:
            head = bytes([0x81, 126]) + n.to_bytes(2, "big")
        else:
            head = bytes([0x81, 127]) + n.to_bytes(8, "big")
        try:
            with self.lock:
                self.sock.sendall(head + data)
        except OSError:
            self.alive = False

    def _control(self, op, payload=b""):
        try:
            with self.lock:
                self.sock.sendall(bytes([0x80 | op, len(payload)]) + payload)
        except OSError:
            self.alive = False

    def _read(self, n):
        b = self.rfile.read(n)
        if len(b) < n:
            raise ConnectionError("closed")
        return b

    def recv(self, limit):
        """Next text message (str), or None when the socket closes."""
        parts = []
        size = 0
        while True:
            b0, b1 = self._read(2)
            op, masked, n = b0 & 0x0F, b1 & 0x80, b1 & 0x7F
            if n == 126:
                n = int.from_bytes(self._read(2), "big")
            elif n == 127:
                n = int.from_bytes(self._read(8), "big")
            if n > limit or size + n > limit:
                return None
            mask = self._read(4) if masked else None
            data = self._read(n)
            if mask and n:
                key = (mask * (n // 4 + 1))[:n]
                data = (int.from_bytes(data, "big") ^ int.from_bytes(key, "big")).to_bytes(n, "big")
            if op == 0x8:
                self._control(0x8)
                return None
            if op == 0x9:
                self._control(0xA, data[:125])
                continue
            if op == 0xA:
                continue
            parts.append(data)
            size += n
            if b0 & 0x80:
                return b"".join(parts).decode("utf-8", "replace")

    def close(self):
        if self.alive:
            self._control(0x8)
        self.alive = False


def _session(sid):
    return _relay.setdefault(sid, {"mac": None, "phones": set(), "layout": None, "state": None})


def _tell_counts(s):
    mac = s["mac"]
    if mac:
        mac.send(f'{{"t":"clients","n":{len(s["phones"])}}}')
    for p in list(s["phones"]):
        p.send(f'{{"t":"mac","on":{"true" if mac else "false"}}}')


def relay_mac(ws, sid):
    with _relay_lock:
        s = _session(sid)
        old, s["mac"] = s["mac"], ws
        _tell_counts(s)
    if old:
        old.close()
    try:
        while True:
            msg = ws.recv(8_000_000)
            if msg is None:
                break
            kind = msg[:24]
            with _relay_lock:
                if '"t":"layout"' in kind:
                    s["layout"] = msg
                elif '"t":"state"' in kind:
                    s["state"] = msg
                else:
                    continue
                phones = list(s["phones"])
            for p in phones:
                p.send(msg)
    except (ConnectionError, OSError, ValueError):
        pass
    finally:
        with _relay_lock:
            if s["mac"] is ws:
                s["mac"] = None
                _tell_counts(s)
            if not s["mac"] and not s["phones"]:
                _relay.pop(sid, None)


def relay_phone(ws, sid, ip):
    with _relay_lock:
        s = _session(sid)
        if len(s["phones"]) >= MAX_PHONES:
            ws.close()
            return
        s["phones"].add(ws)
        first = [m for m in (s["layout"], s["state"]) if m]
        _tell_counts(s)
    for m in first:
        ws.send(m)
    try:
        while True:
            msg = ws.recv(4096)
            if msg is None:
                break
            if not msg.startswith('{"cmd":') or limited(("remote", ip), 3000, 60):
                continue  # pings and junk stay here
            with _relay_lock:
                mac = s["mac"]
            if mac:
                mac.send(msg)
    except (ConnectionError, OSError, ValueError):
        pass
    finally:
        with _relay_lock:
            s["phones"].discard(ws)
            _tell_counts(s)
            if not s["mac"] and not s["phones"]:
                _relay.pop(sid, None)


def list_scripts():
    with db() as c:
        rows = c.execute(
            "SELECT code,title,name,email,created,updated,length(content) AS chars "
            "FROM scripts ORDER BY updated DESC"
        ).fetchall()
    return [dict(r) for r in rows]


def delete_script(code):
    with _db_lock, db() as c:
        c.execute("DELETE FROM scripts WHERE code=?", (code,))


# ---------------------------------------------------------------- http
class H(BaseHTTPRequestHandler):
    server_version = "JSTele/1.0"

    def log_message(self, fmt, *a):
        print(f"{self.ip()} {fmt % a}", flush=True)

    def ip(self):
        return (
            self.headers.get("CF-Connecting-IP")
            or self.headers.get("X-Forwarded-For", "").split(",")[0].strip()
            or self.client_address[0]
        )

    def send(self, status, body, ctype="application/json; charset=utf-8"):
        if isinstance(body, (dict, list)):
            body = json.dumps(body, ensure_ascii=False)
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(body)

    def err(self, status, msg):
        self.send(status, {"error": msg})

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        if n > MAX_CONTENT * 2:
            raise ValueError("too large")
        raw = self.rfile.read(n) if n else b"{}"
        return json.loads(raw.decode("utf-8") or "{}")

    def is_admin(self):
        auth = self.headers.get("Authorization", "")
        return bool(ADMIN_TOKEN) and secrets.compare_digest(auth, f"Bearer {ADMIN_TOKEN}")

    def web_admin(self):
        """Cloudflare Access user allowed into /admin (JWT verified), else None."""
        return access_email(self.headers.get("Cf-Access-Jwt-Assertion", ""))

    def code_from(self, prefix):
        code = self.path.split("?")[0][len(prefix):].strip("/")
        return code if CODE_RE.match(code) else None

    # ---- GET
    def do_HEAD(self):
        self.do_GET()

    def do_GET(self):
        p = self.path.split("?")[0]
        if p == "/" or p.startswith("/s/"):
            with open(os.path.join(HERE, "index.html"), "rb") as f:
                return self.send(200, f.read(), "text/html; charset=utf-8")
        if p == "/healthz":
            return self.send(200, {"ok": True})
        if p.startswith("/r/"):
            if not SID_RE.match(p[3:]):
                return self.err(404, "not found")
            with open(os.path.join(HERE, "remote.html"), "rb") as f:
                return self.send(200, f.read(), "text/html; charset=utf-8")
        if p.startswith("/relay/mac/") or p.startswith("/relay/phone/"):
            role, sid = p[len("/relay/"):].split("/", 1)
            if not SID_RE.match(sid) or self.headers.get("Upgrade", "").lower() != "websocket":
                return self.err(400, "websocket only")
            key = self.headers.get("Sec-WebSocket-Key", "")
            accept = base64.b64encode(hashlib.sha1((key + _WS_GUID).encode()).digest()).decode()
            self.send_response(101, "Switching Protocols")
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            self.wfile.flush()
            self.close_connection = True
            ws = WS(self)
            if role == "mac":
                relay_mac(ws, sid)
            else:
                relay_phone(ws, sid, self.ip())
            return
        if p == "/admin" or p.startswith("/admin/"):
            if not self.web_admin():
                return self.send(403, "Akses ditolak.", "text/plain; charset=utf-8")
            if p.startswith("/admin/api/scripts"):
                return self.send(200, list_scripts())
            with open(os.path.join(HERE, "admin.html"), "rb") as f:
                return self.send(200, f.read(), "text/html; charset=utf-8")
        if p.startswith("/api/admin/scripts"):
            if not self.is_admin():
                return self.err(401, "unauthorized")
            return self.send(200, list_scripts())
        if p.startswith("/api/scripts/"):
            code = self.code_from("/api/scripts/")
            admin = self.is_admin()
            if not admin and limited(("look", self.ip()), 30, 600):
                return self.err(429, "Terlalu banyak cubaan. Cuba lagi dalam 10 minit.")
            if not code:
                return self.err(404, "Kod tidak sah")
            with db() as c:
                r = c.execute("SELECT * FROM scripts WHERE code=?", (code,)).fetchone()
            if not r:
                return self.err(404, "Kod tidak dijumpai")
            return self.send(200, row_public(r))
        self.err(404, "not found")

    # ---- POST
    def do_POST(self):
        p = self.path.split("?")[0]
        try:
            data = self.body()
        except Exception:
            return self.err(400, "JSON tidak sah")
        if p == "/api/scripts":
            if limited(("new", self.ip()), 20, 3600):
                return self.err(429, "Terlalu banyak skrip dihantar. Cuba lagi nanti.")
            title = clean(data.get("title"), 200)
            name = clean(data.get("name"), 120)
            email = clean(data.get("email"), 200).lower()
            content = clean(data.get("content"), MAX_CONTENT)
            if not (title and name and content):
                return self.err(400, "Sila isi tajuk, nama dan skrip.")
            if not EMAIL_RE.match(email):
                return self.err(400, "Email tidak sah.")
            now = int(time.time())
            with _db_lock, db() as c:
                code = new_code(c)
                c.execute(
                    "INSERT INTO scripts VALUES(?,?,?,?,?,?,?)",
                    (code, title, name, email, content, now, now),
                )
            mail_async(email, f"Kod skrip teleprompter anda: {code}", code_email(name, title, code))
            if NOTIFY_EMAIL:
                mail_async(
                    NOTIFY_EMAIL,
                    f"[Tele] Skrip baru {code} — {title}",
                    f"Dari: {name} <{email}>\nKod: {code}\n{PUBLIC_URL}/s/{code}\n\n{content[:2000]}",
                )
            return self.send(201, {"code": code})
        if p == "/api/forgot":
            email = clean(data.get("email"), 200).lower()
            if limited(("forgot", self.ip()), 5, 3600) or limited(("forgot", email), 3, 3600):
                return self.err(429, "Terlalu banyak cubaan. Cuba lagi nanti.")
            if EMAIL_RE.match(email):
                with db() as c:
                    rows = c.execute(
                        "SELECT code,title FROM scripts WHERE email=? ORDER BY updated DESC", (email,)
                    ).fetchall()
                if rows:
                    lines = "\n".join(f"  {r['code']}  —  {r['title']}  ({PUBLIC_URL}/s/{r['code']})" for r in rows)
                    mail_async(
                        email,
                        "Kod skrip teleprompter anda",
                        f"Salam,\n\nBerikut kod skrip yang didaftarkan dengan email ini:\n\n{lines}\n\n— JohorStudio.com\n",
                    )
            # Same answer either way: don't reveal which emails exist.
            return self.send(200, {"ok": True})
        self.err(404, "not found")

    # ---- PUT
    def do_PUT(self):
        p = self.path.split("?")[0]
        if not p.startswith("/api/scripts/"):
            return self.err(404, "not found")
        code = self.code_from("/api/scripts/")
        if not code:
            return self.err(404, "Kod tidak sah")
        if not self.is_admin() and limited(("edit", self.ip()), 120, 600):
            return self.err(429, "Terlalu banyak cubaan.")
        try:
            data = self.body()
        except Exception:
            return self.err(400, "JSON tidak sah")
        content = clean(data.get("content"), MAX_CONTENT)
        title = clean(data.get("title"), 200)
        if not content:
            return self.err(400, "Skrip kosong.")
        with _db_lock, db() as c:
            r = c.execute("SELECT * FROM scripts WHERE code=?", (code,)).fetchone()
            if not r:
                return self.err(404, "Kod tidak dijumpai")
            c.execute(
                "UPDATE scripts SET content=?, title=?, updated=? WHERE code=?",
                (content, title or r["title"], int(time.time()), code),
            )
            r = c.execute("SELECT * FROM scripts WHERE code=?", (code,)).fetchone()
        return self.send(200, row_public(r))

    # ---- DELETE
    def do_DELETE(self):
        p = self.path.split("?")[0]
        if p.startswith("/admin/api/scripts/"):
            who = self.web_admin()
            if not who:
                return self.err(403, "forbidden")
            code = self.code_from("/admin/api/scripts/")
        elif p.startswith("/api/admin/scripts/"):
            if not self.is_admin():
                return self.err(401, "unauthorized")
            who = "app"
            code = self.code_from("/api/admin/scripts/")
        else:
            return self.err(404, "not found")
        if not code:
            return self.err(404, "Kod tidak sah")
        delete_script(code)
        print(f"[admin] {who} deleted {code}", flush=True)
        return self.send(200, {"ok": True})


def main():
    init_db()
    if not ADMIN_TOKEN:
        print("WARNING: ADMIN_TOKEN not set — admin list disabled", flush=True)
    srv = ThreadingHTTPServer(("0.0.0.0", PORT), H)
    srv.daemon_threads = True
    print(f"Teleprompter server on :{PORT}, db={DB_PATH}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
