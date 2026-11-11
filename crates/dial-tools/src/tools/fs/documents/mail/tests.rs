use super::*;

const EML: &[u8] = b"From: Ada Lovelace <ada@example.com>\r\n\
To: team@example.com\r\n\
Date: Sat, 20 Nov 2021 14:22:01 -0800\r\n\
Subject: =?utf-8?q?Quarterly_numbers_=E2=98=95?=\r\n\
Message-ID: <q1@example.com>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Totals are attached =E2=80=94 see row 3.\r\n\
--b\r\n\
Content-Type: text/csv; name=\"totals.csv\"\r\n\
Content-Disposition: attachment; filename=\"totals.csv\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
YSxiCjEsMgo=\r\n\
--b--\r\n";

#[test]
fn message_headers_body_and_attachments_are_decoded() {
    let out = message(EML).unwrap();
    assert!(
        out.contains("From: Ada Lovelace <ada@example.com>"),
        "{out}"
    );
    assert!(out.contains("Subject: Quarterly numbers ☕"), "{out}");
    assert!(out.contains("Date: 2021-11-20T14:22:01-08:00"), "{out}");
    assert!(out.contains("Totals are attached — see row 3."), "{out}");
    assert!(out.contains("totals.csv (text/csv, 8 bytes)"), "{out}");
    assert!(!out.contains("YSxiCjEsMgo="), "{out}");
}

#[test]
fn mbox_lists_messages_and_selects_one() {
    let mut boxed = b"From ada@example.com Sat Nov 20 14:22:01 2021\n".to_vec();
    boxed.extend_from_slice(b"From: ada@example.com\nSubject: first\n\nhello one\n\n");
    boxed.extend_from_slice(b"From bob@example.com Sun Nov 21 09:00:00 2021\n");
    boxed.extend_from_slice(b"From: bob@example.com\nSubject: second\n\nhello two\n");
    let list = mbox(&boxed, None).unwrap();
    assert!(list.starts_with("mbox: 2 messages"), "{list}");
    assert!(list.contains("\tbob@example.com\tsecond\n"), "{list}");
    let second = mbox(&boxed, Some("message=2")).unwrap();
    assert!(
        second.contains("hello two") && !second.contains("hello one"),
        "{second}"
    );
    assert!(
        mbox(&boxed, Some("message=3"))
            .unwrap_err()
            .contains("has 2")
    );
}
