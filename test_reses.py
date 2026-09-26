import unittest

import reses

RAW = b"""Return-Path: <alice@example.com>\r
Received: from mx.example.com by inbound-smtp.example.net with SMTP id abc123\r
 for hidden@example.org;\r
 Sat, 26 Sep 2026 00:02:08 +0000 (UTC)\r
MIME-Version: 1.0\r
From: Alice <alice@example.com>\r
To: bob@example.org\r
Cc: carol@example.org\r
Date: Fri, 25 Sep 2026 17:01:31 -0700\r
Subject: Hello there\r
Message-ID: <id-1@example.com>\r
Content-Type: multipart/mixed; boundary="MIX"\r
\r
--MIX\r
Content-Type: multipart/alternative; boundary="ALT"\r
\r
--ALT\r
Content-Type: text/plain; charset="UTF-8"\r
\r
plain body\r
\r
--ALT\r
Content-Type: text/html; charset="UTF-8"\r
\r
<div>html <b>body</b></div>\r
\r
--ALT--\r
--MIX\r
Content-Type: text/plain; name="notes.txt"\r
Content-Disposition: attachment; filename="notes.txt"\r
\r
hi\r
--MIX--\r
"""


class FormatTest(unittest.TestCase):
    def setUp(self):
        self.msg = reses.parse(RAW)
        self.out = reses.format_message(self.msg)

    def test_headers(self):
        self.assertIn("From: Alice <alice@example.com>\n", self.out)
        self.assertIn("To: bob@example.org\n", self.out)
        self.assertIn("Cc: carol@example.org\n", self.out)
        self.assertIn("Subject: Hello there\n", self.out)
        self.assertIn("Date: Fri, 25 Sep 2026 17:01:31 -0700\n", self.out)

    def test_bcc_from_envelope(self):
        self.assertIn("Bcc: hidden@example.org\n", self.out)

    def test_bcc_excludes_visible_recipients(self):
        raw = RAW.replace(b"for hidden@example.org", b"for bob@example.org")
        self.assertIn("Bcc: \n", reses.format_message(reses.parse(raw)))

    def test_plain_body_and_attachment(self):
        self.assertTrue(self.out.endswith("Message:\n\nplain body\n"))
        self.assertIn("Attachments: notes.txt (2 bytes)\n", self.out)

    def test_html_preference(self):
        self.assertIn("<div>html <b>body</b></div>", reses.format_message(self.msg, prefer_html=True))

    def test_html_only_falls_back_to_text(self):
        raw = (b"From: a@example.com\r\nTo: b@example.com\r\nSubject: s\r\n"
               b"Content-Type: text/html\r\n\r\n<p>one</p><p>two &amp; three</p>\r\n")
        self.assertTrue(reses.format_message(reses.parse(raw)).endswith("one\ntwo & three\n"))


if __name__ == "__main__":
    unittest.main()
