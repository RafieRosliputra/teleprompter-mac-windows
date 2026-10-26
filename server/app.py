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
"""
import json
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
        if p.startswith("/api/admin/scripts"):
            if not self.is_admin():
                return self.err(401, "unauthorized")
            with db() as c:
                rows = c.execute(
                    "SELECT code,title,name,email,created,updated,length(content) AS chars "
                    "FROM scripts ORDER BY updated DESC"
                ).fetchall()
            return self.send(200, [dict(r) for r in rows])
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
        if not self.is_admin():
            return self.err(401, "unauthorized")
        code = self.code_from("/api/admin/scripts/")
        if not code:
            return self.err(404, "Kod tidak sah")
        with _db_lock, db() as c:
            c.execute("DELETE FROM scripts WHERE code=?", (code,))
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
