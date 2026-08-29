#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""Generate the line-oriented and fixed-width fixtures for tdy (job key: logs-fixed-width).

Run from the repo root:  python3 testdata/gen/04_logs_fixedwidth.py
Deterministic and idempotent; stdlib only. Every file it writes is named
testdata/logs_fixed_width_*.

Written files, what each one stresses, and what a correct parse must produce
(full expectations, incl. exact values, are printed to stderr by --report and
duplicated in the job manifest):

1. logs_fixed_width_nginx_access.log  (~25 KB, > sample_bytes = 16 KiB)
   nginx "combined" access log. Stresses Extraction::Lines with real quoting:
   escaped quotes inside the request and user-agent fields (nginx \\x22 / \\ )
   so a naive "([^"]*)" group truncates; a "-" body_bytes_sent on 304s (must
   become NULL, not 0); an IPv6 remote_addr (kills \\d+\\.\\d+\\.\\d+\\.\\d+);
   and 6 malformed lines that NoMatchPolicy::Skip must drop (truncated write,
   an nginx *error* log line, a "common"-format line without referer/UA, a
   line with a trailing space, a line with a broken [time] bracket, an empty
   line).  It is also deliberately larger than config.sample_bytes with every
   comma-bearing line hidden in the unsampled middle: the tier-1 sniffer sees
   a comma-free head+tail, proposes RaggedPolicy::Error, and then its own
   probe over the *whole* file fails -> `tdy sniff` must fail loudly rather
   than emit a spec the engine cannot run (invariant 1).
   Correct parse: Extraction::Lines with NGINX_RE below, on_no_match = skip.

2. logs_fixed_width_syslog.log
   RFC-3164 syslog. Stresses an optional named capture group ([pid] missing on
   kernel/rsyslogd lines -> empty -> NULL int64), space-padded single-digit
   days ("Feb  9"), colons and commas inside the message, a UTF-8 message with
   an em dash and "Grösse"/ß, two RFC-5424 lines in the same file that the
   RFC-3164 pattern must drop, and a Dec 31 -> Jan 1 pair proving the year is
   not recoverable from the file: ts_raw MUST stay utf8 (or be knowingly
   wrong), which is the invariant-6 trap of this fixture.

3. logs_fixed_width_java_app.log  (CRLF line endings)
   Multi-line Java stack traces. Continuation lines must be skipped, and they
   cannot be recognised by indentation: the exception header, "Caused by:" and
   a logged multi-line SQL statement all start at column 0, and one
   continuation line starts with "2026 rows affected" (four digits, like a
   timestamp). Only ^<full timestamp> anchoring gets the row count right.
   Also: a message containing " - " (the logger/message separator) three more
   times, and a Spring-style line with no " - " at all that must be dropped.

4. logs_fixed_width_report_utf8.txt
   Fixed-width report, UTF-8, no BOM. Banner + column header + rules, region
   group headers and Zwischensumme rows interleaved in the body, a GESAMT
   footer, right-aligned integers, right-aligned negative decimals with Swiss
   ' thousands separators, a COBOL-style all-asterisk overflow field, blanks,
   and n/a.  THE POINT: every kunde value contains exactly one 2-byte
   character, so from the "land" column onward the byte offset and the
   character offset of every field disagree by exactly 1 -- and the two
   readings of Extraction::FixedWidth disagree with them:

     FIELDS_CHAR      = what spec.rs documents ("character positions per line
                        after decoding") and what you would count in an editor
     FIELDS_BYTE_UML  = FIELDS_CHAR with every boundary >= 24 shifted by +1,
                        which is what engine::extract_fixed_width actually
                        needs today (it slices line.as_bytes()).

   Whichever of the two the engine implements, this file pins it down, and the
   ASCII control (fixture 5) holds everything else constant.  The damage the
   *wrong* one does is silent, never an error: one customer name is truncated
   to exactly 24 characters and *ends* with the umlaut, so the byte range
   [0,24) cuts a UTF-8 sequence in half and from_utf8_lossy yields U+FFFD;
   every numeric column is right-aligned, so a one-character slide drops each
   value's last digit and still parses (1234 -> 123, 84'320.57 -> 84'320.5,
   "CH" -> "C"), and menge=8 on the truncated-name row slides into pure
   whitespace and becomes NULL. Exact right/wrong values are printed to stderr
   by this script and repeated in the manifest (invariant 6).  The printed
   Zwischensumme/GESAMT rows are consistent with the body: sum(menge) =
   1505918 and sum(betrag_chf) = 1'574'559.68 over the 9 kept rows, so a test
   can cross-check the parse against the report's own footer.

5. logs_fixed_width_report_ascii.txt
   The same layout, the same numbers, the same row count, ASCII-only names:
   the control. Here character offsets == byte offsets, so FIELDS_CHAR is
   unambiguously correct however the engine slices. Any difference in
   behaviour between 4 and 5 is the encoding axis, nothing else.

6. logs_fixed_width_report_cp1252.txt
   Same visible report as fixture 4, encoded windows-1252: the file is
   byte-aligned *on disk* (one byte per umlaut), but both tiers slice the
   *decoded* buffer, so the on-disk offsets are correct for neither reading -
   the answer is FIELDS_CHAR or FIELDS_BYTE_UML exactly as in fixture 4. Also
   forces extraction.encoding = "windows-1252": there is no BOM, and the file
   holds only 12 non-ASCII bytes for chardetng to go on.
"""

import os
import random
import re
import sys
from datetime import datetime, timedelta

random.seed(20260211)

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
OUT = os.path.join(REPO, "testdata")
PREFIX = "logs_fixed_width_"

MON = ["Jan", "Feb", "Mar", "Apr", "May", "Jun",
       "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"]


def log(msg):
    print(msg, file=sys.stderr)


def write_bytes(name, data):
    assert name.startswith(PREFIX), name
    path = os.path.join(OUT, name)
    with open(path, "wb") as fh:
        fh.write(data)
    print("wrote testdata/%s (%d bytes)" % (name, len(data)))
    return path


# ---------------------------------------------------------------------------
# 1. nginx combined access log
# ---------------------------------------------------------------------------

NGINX_RE = re.compile(
    r'^(?P<remote_addr>\S+) (?P<ident>\S+) (?P<remote_user>\S+) '
    r'\[(?P<ts>[^ \]]+) (?P<tz>[+-]\d{4})\] '
    r'"(?P<request>(?:[^"\\]|\\.)*)" (?P<status>\d{3}) (?P<bytes>\d+|-) '
    r'"(?P<referer>(?:[^"\\]|\\.)*)" "(?P<user_agent>(?:[^"\\]|\\.)*)"$'
)

# every one of these is comma-free on purpose
UAS_NO_COMMA = [
    "curl/8.6.0",
    "Wget/1.21.4",
    "python-requests/2.31.0",
    "Mozilla/5.0 (iPhone; CPU iPhone OS 17_2 like Mac OS X)",
    "Mozilla/5.0 (X11; Linux x86_64; rv:121.0) Gecko/20100101 Firefox/121.0",
    "Prometheus/2.49.1",
]
PATHS = [
    "/", "/index.html", "/de/produkte", "/de/produkte/widget",
    "/static/app.4f2c91.js", "/static/theme.css", "/api/v1/health",
    "/api/v1/kunden?page=3", "/robots.txt", "/de/kontakt",
]
REFERERS = ["-", "https://example.ch/de/produkte", "https://www.google.ch/",
            "https://example.ch/index.html"]
IPS = ["203.0.113.7", "203.0.113.9", "198.51.100.4", "198.51.100.23",
       "192.0.2.44", "203.0.113.201"]


def nginx_line(ip, user, dt, tz, req, status, nbytes, ref, ua):
    stamp = "%02d/%s/%d:%02d:%02d:%02d" % (dt.day, MON[dt.month - 1], dt.year,
                                           dt.hour, dt.minute, dt.second)
    return '%s - %s [%s %s] "%s" %s %s "%s" "%s"' % (
        ip, user, stamp, tz, req, status, nbytes, ref, ua)


def build_nginx():
    clock = [datetime(2026, 2, 11, 8, 15, 0)]

    def tick():
        clock[0] += timedelta(seconds=random.randint(1, 37))
        return clock[0]

    def filler():
        return nginx_line(
            random.choice(IPS), "-", tick(), "+0100",
            "%s %s HTTP/1.1" % (random.choice(["GET", "GET", "GET", "HEAD", "POST"]),
                                random.choice(PATHS)),
            random.choice([200, 200, 200, 200, 301, 302, 404, 500]),
            random.randint(180, 98304),
            random.choice(REFERERS), random.choice(UAS_NO_COMMA))

    head = []
    # line 1 is the documented "row 0" of the expected output
    head.append(nginx_line("203.0.113.7", "-", clock[0], "+0100",
                           "GET /index.html HTTP/1.1", 200, 5120, "-", "curl/8.6.0"))
    # 304 with "-" body_bytes_sent -> must be NULL, never 0
    head.append(nginx_line("203.0.113.7", "-", tick(), "+0100",
                           "GET /static/theme.css HTTP/1.1", 304, "-",
                           "https://example.ch/index.html", "curl/8.6.0"))
    # authenticated user + uncommon method + 207
    head.append(nginx_line("198.51.100.23", "svc_report", tick(), "+0100",
                           "PROPFIND /dav/reports/2026-02 HTTP/1.1", 207, 41213,
                           "-", "Wget/1.21.4"))
    # IPv6 remote_addr
    head.append(nginx_line("2001:db8:85a3::8a2e:370:7334", "-", tick(), "+0100",
                           "GET /de/produkte HTTP/1.1", 200, 18422,
                           "https://www.google.ch/", "Prometheus/2.49.1"))
    # MALFORMED: truncated write (client disconnected mid-log)
    head.append('203.0.113.9 - - [11/Feb/2026:08:16:44 +0100] "GET /de/kont')
    while sum(len(l) + 1 for l in head) < 13600:
        head.append(filler())

    mid = []
    # comma-bearing (and therefore sniffer-poisoning) lines live only here
    mid.append(nginx_line("192.0.2.44", "-", tick(), "+0100",
                          "GET /de/produkte/widget HTTP/1.1", 200, 22110,
                          "https://example.ch/de/produkte",
                          "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                          "(KHTML, like Gecko) Chrome/121.0.0.0 Safari/537.36"))
    # escaped quotes inside the user agent (nginx escapes " as \x22)
    mid.append(nginx_line("203.0.113.201", "-", tick(), "+0100",
                          "GET /robots.txt HTTP/1.1", 200, 312, "-",
                          "Mozilla/5.0 (compatible; \\x22GreedyBot\\x22/2.1; "
                          "+http://bot.example/faq)"))
    # escaped quotes inside the request line
    mid.append(nginx_line("198.51.100.4", "-", tick(), "+0100",
                          "GET /suche?q=\\x22tidy%20data\\x22&lang=de HTTP/1.1",
                          200, 9044, "-", "curl/8.6.0"))
    # raw TLS handshake logged as a request line: backslashes everywhere
    mid.append(nginx_line("192.0.2.44", "-", tick(), "+0100",
                          "\\x16\\x03\\x01\\x02\\x00\\x01\\x00\\x01\\xFC\\x03\\x03",
                          400, 157, "-", "-"))
    # huge body_bytes_sent (> 2^31)
    mid.append(nginx_line("203.0.113.7", "-", tick(), "+0100",
                          "GET /downloads/dump-2026-02.tar HTTP/1.1", 200,
                          5368709120, "-", "Wget/1.21.4"))
    # nginx-specific 499 (client closed request)
    mid.append(nginx_line("198.51.100.23", "-", tick(), "+0100",
                          "POST /api/v1/import HTTP/1.1", 499, 0, "-",
                          "python-requests/2.31.0"))
    # MALFORMED: an nginx *error* log line that got merged into the access log
    mid.append('2026/02/11 09:02:17 [error] 1234#0: *55 open() "/var/www/x" failed '
               '(2: No such file or directory), client: 203.0.113.9, server: example.ch')
    # MALFORMED: "common" log format - no referer, no user agent
    mid.append(nginx_line("198.51.100.4", "-", tick(), "+0100",
                          "GET /favicon.ico HTTP/1.1", 200, 1406, "-", "-")
               .split(' "-" "-"')[0])
    # MALFORMED: broken [time] bracket
    mid.append('203.0.113.9 - - 11/Feb/2026:09:03:02 +0100 "GET / HTTP/1.1" 200 512 "-" "curl/8.6.0"')
    # MALFORMED: trailing space after the final quote -> $ does not match
    mid.append(nginx_line("203.0.113.9", "-", tick(), "+0100",
                          "GET /de/kontakt HTTP/1.1", 200, 7781, "-", "curl/8.6.0") + " ")
    # MALFORMED: empty line (dropped by the extractor before the pattern runs)
    mid.append("")
    # more comma-bearing Chrome lines, still inside the unsampled middle
    for _ in range(8):
        mid.append(nginx_line(random.choice(IPS), "-", tick(), "+0100",
                              "GET %s HTTP/1.1" % random.choice(PATHS),
                              200, random.randint(500, 40000),
                              random.choice(REFERERS),
                              "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) "
                              "AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.3 Safari/605.1.15"))

    tail = []
    while sum(len(l) + 1 for l in tail) < 4600:
        tail.append(filler())
    # a different timezone offset
    tail.append(nginx_line("192.0.2.44", "-", tick(), "-0500",
                           "GET /api/v1/health HTTP/1.1", 200, 61, "-",
                           "Prometheus/2.49.1"))
    # request field is "-" (connection dropped before the request line arrived)
    tail.append(nginx_line("203.0.113.9", "-", tick(), "+0100",
                           "-", 400, 0, "-", "-"))
    # referer containing a raw space
    tail.append(nginx_line("198.51.100.4", "-", tick(), "+0100",
                           "GET /de/kontakt HTTP/1.1", 200, 7781,
                           "https://example.ch/de/a b", "curl/8.6.0"))

    lines = head + mid + tail
    text = "\n".join(lines) + "\n"
    data = text.encode("utf-8")

    # --- self-checks -------------------------------------------------------
    sample_bytes = 16 * 1024
    head_len = min(len(data), sample_bytes * 3 // 4)      # 12288
    tail_start = len(data) - min(sample_bytes // 4, len(data) - head_len)
    off = 0
    for line in lines:
        n = len(line.encode("utf-8"))
        if "," in line:
            assert off >= head_len and off + n < tail_start, \
                "comma-bearing line must sit in the unsampled middle: %r" % line[:60]
        off += n + 1
    matched = [l for l in lines if l.strip() and NGINX_RE.match(l)]
    dropped = [l for l in lines if l.strip() and not NGINX_RE.match(l)]
    assert len(dropped) == 5, dropped
    m0 = NGINX_RE.match(lines[0])
    assert m0.group("remote_addr") == "203.0.113.7"
    assert m0.group("ts") == "11/Feb/2026:08:15:00"
    assert m0.group("bytes") == "5120"
    nulls = [l for l in matched if NGINX_RE.match(l).group("bytes") == "-"]
    log("[nginx] lines=%d matched=%d dropped=%d bytes-null=%d head_len=%d tail_start=%d total=%d"
        % (len(lines), len(matched), len(dropped), len(nulls), head_len, tail_start, len(data)))
    log("[nginx] row0 = %s" % {k: v for k, v in m0.groupdict().items()})
    last = NGINX_RE.match(matched[-1])
    log("[nginx] last row = %s" % {k: v for k, v in last.groupdict().items()})
    log("[nginx] status set = %s" % sorted({NGINX_RE.match(l).group("status") for l in matched}))
    log("[nginx] max bytes = %s" % max(int(NGINX_RE.match(l).group("bytes"))
                                       for l in matched
                                       if NGINX_RE.match(l).group("bytes") != "-"))
    return write_bytes(PREFIX + "nginx_access.log", data), len(matched), len(dropped)


# ---------------------------------------------------------------------------
# 2. syslog (RFC 3164)
# ---------------------------------------------------------------------------

SYSLOG_RE = re.compile(
    r'^(?P<ts_raw>[A-Z][a-z]{2} +\d{1,2} \d{2}:\d{2}:\d{2}) '
    r'(?P<host>\S+) (?P<proc>[^\s\[\]:]+)(?:\[(?P<pid>\d+)\])?: (?P<message>.*)$'
)

SYSLOG_LINES = [
    # single-digit day is space padded -> two spaces
    "Feb  9 03:12:44 web01 sshd[2411]: Accepted publickey for deploy from 203.0.113.7 port 55012 ssh2",
    "Feb  9 03:12:44 web01 sshd[2411]: pam_unix(sshd:session): session opened for user deploy(uid=1001)",
    # kernel: no [pid] at all -> pid must be NULL
    "Feb 10 03:12:45 web01 kernel: [12345.678901] TCP: request_sock_TCP: Possible SYN flooding on port 443",
    "Feb 10 03:12:46 web01 systemd[1]: Started Session 42 of user deploy.",
    "Feb 10 03:12:46 web01 CRON[9911]: (root) CMD (/usr/lib/sysstat/debian-sa1 1 1)",
    # colons and commas inside the message
    "Feb 10 03:13:01 db01 postgres[1123]: [3-1] user=app,db=prod LOG:  duration: 1234.567 ms  statement: SELECT 1",
    # no pid, and the message is itself a syslog artefact
    "Feb 10 03:13:02 web01 rsyslogd: --- last message repeated 3 times ---",
    # hostname and process name with dashes
    "Feb 10 03:13:30 edge-01 nginx-ingress[2]: 203.0.113.9 - - upstream_response_time 0.031",
    # UTF-8 message: em dash, umlaut, sharp s
    "Feb 10 03:14:00 web01 backup[7788]: Sicherung abgeschlossen — 3 Volumes, 0 Fehler (Größe: 12 GiB)",
    "Feb 10 03:14:05 web01 backup[7788]: Nächster Lauf: 11.02.2026 03:14",
    # RFC 5424 lines mixed into the same file: must NOT match the 3164 pattern
    "2026-02-10T03:15:30.412345+01:00 web01 auditd[812]: type=USER_LOGIN msg=audit(1770689730.412:77)",
    "2026-02-10T03:15:31.004112+01:00 web01 auditd[812]: type=CRED_ACQ msg=audit(1770689731.004:78)",
    # truncated line
    "Feb 10 03:15:0",
    "Feb 10 03:16:12 web01 sudo[3312]:  deploy : TTY=pts/0 ; PWD=/srv/app ; USER=root ; COMMAND=/bin/systemctl restart app",
    "Feb 10 03:16:13 web01 systemd[1]: Stopping Acme App Server...",
    "Feb 10 03:16:19 web01 systemd[1]: app.service: Succeeded.",
    "Feb 10 03:16:19 web01 systemd[1]: Started Acme App Server.",
    "Feb 10 03:17:02 db01 postgres[1123]: [4-1] user=,db= LOG:  checkpoint complete: wrote 1204 buffers (7.3%)",
    "Feb 10 03:18:00 web01 kernel: [12456.001122] audit: type=1400 audit(1770689880.001:91): apparmor=\"DENIED\"",
    # the year trap: Dec 31 immediately followed by Jan 1
    "Dec 31 23:59:59 web01 ntpd[555]: kernel time sync status change 2001",
    "Jan  1 00:00:02 web01 ntpd[555]: kernel time sync status change 0001",
    "Jan  1 00:00:07 web01 systemd[1]: logrotate.service: Deactivated successfully.",
]


def build_syslog():
    text = "\n".join(SYSLOG_LINES) + "\n"
    data = text.encode("utf-8")
    matched = [l for l in SYSLOG_LINES if l.strip() and SYSLOG_RE.match(l)]
    dropped = [l for l in SYSLOG_LINES if l.strip() and not SYSLOG_RE.match(l)]
    assert len(dropped) == 3, dropped
    m0 = SYSLOG_RE.match(SYSLOG_LINES[0])
    assert m0.group("ts_raw") == "Feb  9 03:12:44"
    assert m0.group("pid") == "2411"
    no_pid = [l for l in matched if SYSLOG_RE.match(l).group("pid") is None]
    assert len(no_pid) == 3, no_pid
    log("[syslog] lines=%d matched=%d dropped=%d pid-null=%d"
        % (len(SYSLOG_LINES), len(matched), len(dropped), len(no_pid)))
    log("[syslog] row0 = %s" % m0.groupdict())
    log("[syslog] em-dash row = %s" % SYSLOG_RE.match(SYSLOG_LINES[8]).groupdict())
    log("[syslog] last row = %s" % SYSLOG_RE.match(matched[-1]).groupdict())
    return write_bytes(PREFIX + "syslog.log", data), len(matched), len(dropped)


# ---------------------------------------------------------------------------
# 3. Java application log with multi-line stack traces (CRLF)
# ---------------------------------------------------------------------------

JAVA_RE = re.compile(
    r'^(?P<ts>\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}),(?P<millis>\d{3}) '
    r'(?P<level>[A-Z]+) +\[(?P<thread>[^\]]+)\] (?P<logger>\S+) - (?P<message>.*)$'
)

JAVA_LINES = [
    "2026-02-10 03:14:02,117 INFO  [main] c.a.batch.JobRunner - starting job=nightly-rollup args=[--full --tenant=ch]",
    "2026-02-10 03:14:03,004 DEBUG [main] c.a.db.Pool - pool created min=4 max=32",
    # message contains the ' - ' field separator three more times
    "2026-02-10 03:14:05,551 INFO  [http-nio-8080-exec-3] c.a.http.Client - GET /v1/tenants - 200 - 41ms - cached",
    "2026-02-10 03:14:07,884 WARN  [pool-2-thread-1] c.a.batch.Retry - attempt 1/3 failed for tenant=ch; retrying in 2s",
    "2026-02-10 03:14:12,003 ERROR [pool-2-thread-1] c.a.batch.JobRunner - job=nightly-rollup failed after 9884 ms",
    # continuation block: NOT indented on the first line, so indentation is no test
    "java.lang.IllegalStateException: connection pool exhausted (active=32, idle=0, waiting=17)",
    "\tat com.acme.db.Pool.borrow(Pool.java:214) ~[app-2026.02.09.jar:2026.02.09]",
    "\tat com.acme.batch.JobRunner.run(JobRunner.java:88) ~[app-2026.02.09.jar:2026.02.09]",
    "\tat java.base/java.util.concurrent.ThreadPoolExecutor.runWorker(ThreadPoolExecutor.java:1144)",
    "\t... 24 common frames omitted",
    "Caused by: java.net.SocketTimeoutException: connect timed out",
    "\tat java.base/java.net.Socket.connect(Socket.java:633) ~[na:na]",
    "\t... 31 common frames omitted",
    # a real event again
    "2026-02-10 03:15:00,001 INFO  [scheduler-1] c.a.sched.Cron - next fire time 2026-02-11 03:15:00,000 for job=nightly-rollup",
    # multi-line SQL logged as one event: continuations start at column 0
    "2026-02-10 03:16:20,455 DEBUG [pool-2-thread-3] c.a.db.Sql - executing:",
    "SELECT kunde, sum(betrag)",
    "  FROM umsatz",
    " WHERE monat = '2026-02'",
    " GROUP BY kunde",
    "2026 rows affected",
    # Spring-style line with no ' - ' separator at all -> dropped
    "2026-02-10 03:16:21,900 INFO  [main] Started AcmeApplication in 4.213 seconds (process running for 4.9)",
    "2026-02-10 03:16:22,010 TRACE [pool-2-thread-3] c.a.db.Sql - rows=2026 elapsed=1.884s",
    "2026-02-10 03:17:41,742 FATAL [main] c.a.batch.JobRunner - unrecoverable: shutting down",
    "2026-02-10 03:17:41,743 INFO  [Thread-4] c.a.Shutdown - bye",
]


def build_java():
    text = "\r\n".join(JAVA_LINES) + "\r\n"
    data = text.encode("utf-8")
    # Rust's str::lines() strips the trailing \r, so match on the bare lines
    matched = [l for l in JAVA_LINES if l.strip() and JAVA_RE.match(l)]
    dropped = [l for l in JAVA_LINES if l.strip() and not JAVA_RE.match(l)]
    m0 = JAVA_RE.match(JAVA_LINES[0])
    assert m0.group("ts") == "2026-02-10 03:14:02"
    assert m0.group("millis") == "117"
    assert m0.group("thread") == "main"
    assert m0.group("logger") == "c.a.batch.JobRunner"
    sep = JAVA_RE.match(JAVA_LINES[2])
    assert sep.group("message") == "GET /v1/tenants - 200 - 41ms - cached", sep.group("message")
    assert not JAVA_RE.match("2026 rows affected")
    assert not JAVA_RE.match(JAVA_LINES[20])          # the Spring line
    log("[java] lines=%d matched=%d dropped=%d" % (len(JAVA_LINES), len(matched), len(dropped)))
    log("[java] row0 = %s" % m0.groupdict())
    log("[java] levels = %s" % [JAVA_RE.match(l).group("level") for l in matched])
    log("[java] last row = %s" % JAVA_RE.match(matched[-1]).groupdict())
    return write_bytes(PREFIX + "java_app.log", data), len(matched), len(dropped)


# ---------------------------------------------------------------------------
# 4-6. fixed-width report: UTF-8 / ASCII control / windows-1252
# ---------------------------------------------------------------------------
#
# character layout (what you see in an editor)
#   kunde       [ 0, 24)   left, hard-truncated at 24 characters
#   land        [24, 26)   left, immediately adjacent to kunde: no separator
#   (gutter 26)
#   menge       [27, 34)   right-aligned integer
#   (gutter 34)
#   betrag_chf  [35, 48)   right-aligned decimal, Swiss ' thousands, leading -
#   (gutter 48)
#   abweichung  [49, 60)   right-aligned, (parentheses) mean negative
#   (gutter 60)
#   marge_pct   [61, 68)   right-aligned percentage with a trailing %
#   (gutter 68)
#   bemerkung   [69, ..)   free text to end of line
#
# In the UTF-8/cp1252 files every kunde holds exactly one non-ASCII character,
# so the correct BYTE offsets are the above +1 from `land` onwards.

FIELDS_CHAR = [
    ("kunde", 0, 24), ("land", 24, 26), ("menge", 27, 34), ("betrag_chf", 35, 48),
    ("abweichung", 49, 60), ("marge_pct", 61, 68), ("bemerkung", 69, 9999),
]
FIELDS_BYTE_UML = [
    ("kunde", 0, 25), ("land", 25, 27), ("menge", 28, 35), ("betrag_chf", 36, 49),
    ("abweichung", 50, 61), ("marge_pct", 62, 69), ("bemerkung", 70, 9999),
]

# (kunde_utf8, kunde_ascii, land, menge, betrag, abweichung, marge, bemerkung_utf8, bemerkung_ascii)
ROWS = [
    ("Müller Transport AG", "Mueller Transport AG", "CH", "1234", "84'320.57",
     "(1'250.10)", "12.5%", "Rahmenvertrag 2026", "Rahmenvertrag 2026"),
    ("Bäckerei Steiner", "Baeckerei Steiner", "CH", "96", "-8'450.23",
     "310.40", "4.0%", "Rücklieferung 2 Paletten", "Ruecklieferung 2 Paletten"),
    # name truncated to exactly 24 characters, ending ON the umlaut
    ("Genossenschaft Zentral Ölmühle", "Genossenschaft Zentral Oelmuehle", "", "8", "12'000.99",
     "0.00", "n/a", "Ausschreibung offen", "Ausschreibung offen"),
    ("Zürich Versicherung", "Zuerich Versicherung", "CH", "4500", "1'234'567.89",
     "(12.35)", "0.8%", "Grossbezug — Rabatt 3%", "Grossbezug - Rabatt 3%"),
    ("Gemüsebau Seeland", "Gemuesebau Seeland", "CH", "", "2'100.45",
     "45.00", "7.25%", "Menge unbekannt", "Menge unbekannt"),
    ("Weinhandlung Rüegg", "Weinhandlung Rueegg", "AT", "77", "*************",
     "5.00", "15.0%", "Betrag ueberlaeuft die Spalte", "Betrag ueberlaeuft die Spalte"),
    ("Bürgi Elektro GmbH", "Buergi Elektro GmbH", "DE", "2", "19.95",
     "(0.10)", "33.3%", "", ""),
    ("Schwyzer Kaffeerösterei", "Schwyzer Kaffeeroesterei", "CH", "1500000", "-0.05",
     "0.00", "0.0%", "Gutschrift Rundung", "Gutschrift Rundung"),
    ("Käser Immobilien AG", "Kaeser Immobilien AG", "LI", "1", "250'000.11",
     "(99'999.99)", "100.0%", "Objektverkauf", "Objektverkauf"),
]


def render(kunde, land, menge, betrag, abw, marge, bem):
    line = "%-24s%-2s %7s %13s %11s %7s %s" % (kunde[:24], land, menge, betrag,
                                               abw, marge, bem)
    return line.rstrip()


def report_lines(ascii_only):
    k = 1 if ascii_only else 0
    bem = 8 if ascii_only else 7
    lines = []
    lines.append("%-52s%s" % ("Muster Handels AG", "Seite 1 von 1"))
    lines.append("%-52s%s" % ("Umsatzstatistik nach Kunde", "Stand: 10.02.2026"))
    lines.append("")
    lines.append(render("Kunde", "LD", "Menge", "Betrag CHF", "Abweichung", "Marge",
                        "Bemerkung"))
    lines.append("-" * 98)
    lines.append("Region Ost")
    for r in ROWS[:4]:
        lines.append(render(r[k], r[2], r[3], r[4], r[5], r[6], r[bem]))
    lines.append(render("  Zwischensumme Ost", "", "5838", "1'322'439.22", "", "", ""))
    lines.append("")
    lines.append("Region West")
    for r in ROWS[4:]:
        lines.append(render(r[k], r[2], r[3], r[4], r[5], r[6], r[bem]))
    lines.append(render("  Zwischensumme West", "", "1500080", "252'120.46", "", "", ""))
    lines.append("-" * 98)
    total_label = "GESAMT (alle Laender)" if ascii_only else "GESAMT (alle Länder)"
    lines.append(render(total_label, "", "1505918", "1'574'559.68", "", "", ""))
    lines.append("")
    lines.append("* Werte in CHF. ** (Klammern) = negativ. *** Betrag zu breit fuer die Spalte;")
    lines.append("  die Summen enthalten diese Position nicht.")
    return lines


def slice_fields(line_bytes, fields):
    out = {}
    for name, s, e in fields:
        s2, e2 = min(s, len(line_bytes)), min(e, len(line_bytes))
        out[name] = line_bytes[s2:e2].decode("utf-8", "replace").strip()
    return out


def engine_rows(data, fields, encoding_label):
    """Mimic engine::extract_fixed_width: decode, drop blank lines, slice bytes."""
    text = data.decode(encoding_label)
    rows = []
    for line in text.split("\n"):
        line = line.rstrip("\r")
        if not line.strip():
            continue
        rows.append(slice_fields(line.encode("utf-8"), fields))
    return rows


def check_report(data, encoding_label, ascii_only, tag, fields=None):
    if fields is None:
        fields = FIELDS_CHAR if ascii_only else FIELDS_BYTE_UML
    rows = engine_rows(data, fields, encoding_label)
    assert len(rows) == 21, (tag, len(rows))
    body = rows[4:-4]                      # skip_rows head=4 tail=4
    assert len(body) == 13, (tag, len(body))
    body = [r for r in body if not re.match(r"^(Region|Zwischensumme)\b", r["kunde"])]
    assert len(body) == 9, (tag, len(body))
    k = 1 if ascii_only else 0
    for i, (want, got) in enumerate(zip(ROWS, body)):
        assert got["kunde"] == want[k][:24], (tag, i, got["kunde"], want[k][:24])
        assert got["land"] == want[2], (tag, i, got["land"], want[2])
        assert got["menge"] == want[3], (tag, i, got["menge"], want[3])
        assert got["betrag_chf"] == want[4], (tag, i, got["betrag_chf"], want[4])
        assert got["abweichung"] == want[5], (tag, i, got["abweichung"])
        assert got["marge_pct"] == want[6], (tag, i, got["marge_pct"])
    for i in (0, 2, 7):
        log("[%s] %s -> row%d=%s" % (tag, "FIELDS_CHAR" if fields is FIELDS_CHAR
                                     else "FIELDS_BYTE_UML", i, body[i]))
    return body


def build_reports():
    out = {}

    # --- UTF-8 -------------------------------------------------------------
    lines = report_lines(ascii_only=False)
    for r in ROWS:
        name = r[0][:24]
        non_ascii = [c for c in name if ord(c) > 127]
        assert len(non_ascii) == 1, (name, non_ascii)
    assert len(ROWS[2][0][:24]) == 24 and ord(ROWS[2][0][23]) > 127, ROWS[2][0]
    text = "\n".join(lines) + "\n"
    data_utf8 = text.encode("utf-8")
    assert not data_utf8.startswith(b"\xef\xbb\xbf")
    p_utf8 = write_bytes(PREFIX + "report_utf8.txt", data_utf8)
    good = check_report(data_utf8, "utf-8", False, "utf8")

    # What the *other* reading silently produces instead. Today's engine
    # slices bytes, so FIELDS_CHAR is the wrong one and this is the damage;
    # if fixed_width is changed to slice characters the two swap roles and
    # FIELDS_BYTE_UML produces the mirror-image garbage.
    bad_rows = engine_rows(data_utf8, FIELDS_CHAR, "utf-8")
    bad = [r for r in bad_rows[4:-4] if not re.match(r"^(Region|Zwischensumme)\b", r["kunde"])]
    assert len(bad) == 9
    assert bad[2]["kunde"] == "Genossenschaft Zentral �", bad[2]["kunde"]
    assert bad[0]["menge"] == "123" and good[0]["menge"] == "1234"
    assert bad[0]["betrag_chf"] == "84'320.5" and good[0]["betrag_chf"] == "84'320.57"
    assert bad[0]["land"] == "C" and good[0]["land"] == "CH"
    for i in (0, 2, 7):
        log("[utf8] WRONG (FIELDS_CHAR on a byte-slicing engine) -> row%d=%s" % (i, bad[i]))
    out["utf8"] = p_utf8

    # --- ASCII control -----------------------------------------------------
    lines_a = report_lines(ascii_only=True)
    text_a = "\n".join(lines_a) + "\n"
    data_ascii = text_a.encode("ascii")
    p_ascii = write_bytes(PREFIX + "report_ascii.txt", data_ascii)
    ctl = check_report(data_ascii, "ascii", True, "ascii")
    assert ctl[0]["menge"] == "1234" and ctl[0]["betrag_chf"] == "84'320.57"
    assert ctl[2]["kunde"] == "Genossenschaft Zentral O"
    out["ascii"] = p_ascii

    # --- windows-1252 ------------------------------------------------------
    data_cp = text.encode("cp1252")
    assert len(data_cp) < len(data_utf8)
    p_cp = write_bytes(PREFIX + "report_cp1252.txt", data_cp)
    check_report(data_cp, "cp1252", False, "cp1252")
    # on disk one byte per umlaut, so raw-byte slicing with FIELDS_CHAR would
    # work -- but nothing ever slices the raw bytes; the decoded buffer is
    # what both tiers see, and there FIELDS_BYTE_UML applies exactly as in the
    # UTF-8 fixture.
    on_disk = [l for l in data_cp.split(b"\n") if l.strip()][5]
    assert slice_fields(on_disk, FIELDS_CHAR)["menge"] == "1234"
    assert len([b for b in data_cp if b > 127]) == 12
    log("[cp1252] raw-byte offsets == FIELDS_CHAR, decoded offsets == FIELDS_BYTE_UML; "
        "non-ASCII bytes in the whole file: 12")
    out["cp1252"] = p_cp
    return out


def main():
    os.makedirs(OUT, exist_ok=True)
    build_nginx()
    build_syslog()
    build_java()
    build_reports()


if __name__ == "__main__":
    main()
