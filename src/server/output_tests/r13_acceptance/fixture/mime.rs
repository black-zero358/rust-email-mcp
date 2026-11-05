use sha2::{Digest, Sha256};

pub(super) const ATTACHMENT_UID_STRIDE: u32 = 10;

const PLAIN_MIME: &[u8] =
    b"Content-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n";
const HTML_MIME: &[u8] =
    b"Content-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\n\r\n";
const ALTERNATIVE_MIME: &[u8] = b"Content-Type: multipart/alternative; boundary=alt-r13\r\n\r\n";
const BINARY_MIME: &[u8] = b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=sample.bin\r\nContent-Transfer-Encoding: binary\r\n\r\n";

pub(super) struct Message {
    pub header: Vec<u8>,
    text: Vec<u8>,
    html: Option<Vec<u8>>,
    attachment: Option<Payload>,
}

#[derive(Clone, Copy)]
pub(super) struct Payload {
    pub bytes: usize,
    seed: u8,
}

impl Payload {
    fn fill(self, output: &mut Vec<u8>, offset: usize, count: usize) {
        let pattern: [u8; 251] = std::array::from_fn(|index| (index as u8).wrapping_add(self.seed));
        let mut start = offset % pattern.len();
        let mut remaining = count;
        while remaining != 0 {
            let n = remaining.min(pattern.len() - start);
            output.extend_from_slice(&pattern[start..start + n]);
            remaining -= n;
            start = 0;
        }
    }

    pub fn sha256(self) -> String {
        let mut hasher = Sha256::new();
        let mut bytes = Vec::with_capacity(64 * 1024);
        for offset in (0..self.bytes).step_by(64 * 1024) {
            bytes.clear();
            self.fill(&mut bytes, offset, (self.bytes - offset).min(64 * 1024));
            hasher.update(&bytes);
        }
        format!("{:x}", hasher.finalize())
    }
}

pub(super) fn large(account: usize, uid: u32) -> Payload {
    let bytes = match (account, uid) {
        (_, 1 | 2) => 16 * 1024 * 1024,
        (0, 3) => 65_537,
        (1, 3) => 32 * 1024 * 1024,
        _ => unreachable!(),
    };
    Payload {
        bytes,
        seed: 7 * account as u8 + uid as u8,
    }
}

#[derive(Clone, Copy)]
enum Piece<'a> {
    Bytes(&'a [u8]),
    Attachment(Payload),
}

impl Piece<'_> {
    fn len(self) -> usize {
        match self {
            Self::Bytes(bytes) => bytes.len(),
            Self::Attachment(payload) => payload.bytes,
        }
    }

    fn write(self, output: &mut Vec<u8>, start: usize, count: usize) {
        match self {
            Self::Bytes(bytes) => output.extend_from_slice(&bytes[start..start + count]),
            Self::Attachment(payload) => payload.fill(output, start, count),
        }
    }
}

impl Message {
    pub fn new(account: usize, mailbox: &str, uid: u32) -> Self {
        let identity = if uid <= 5 && mailbox != "Downloads" {
            "copy"
        } else {
            mailbox
        };
        let label = ["alpha", "beta"][account];
        let mut header = format!(
            "From: Synthetic {label} <sender@example.test>\r\nTo: reader@example.test\r\n\
             Subject: {label} {identity} {uid} 合成邮件 {}\r\n {}\r\n\
             Message-ID: <{label}-{identity}-{uid}@example.test>\r\n",
            "长中文标题".repeat(42),
            "续行边界".repeat(48),
        );
        if (2..=5).contains(&uid) && identity == "copy" {
            header.push_str(&format!("References: <{label}-copy-1@example.test>\r\nIn-Reply-To: <{label}-copy-1@example.test>\r\n"));
        }
        let html = (mailbox != "Downloads" && uid.is_multiple_of(ATTACHMENT_UID_STRIDE)).then(|| {
            format!("<p>合成邮件 {label} {uid} <a href=\"https://example.test/a\">样例链接</a></p><table><tr><th>名字</th><td>值</td></tr></table>\r\n").into_bytes()
        });
        let attachment = if mailbox == "Downloads" {
            Some(large(account, uid))
        } else {
            html.as_ref().map(|_| small(uid))
        };
        if attachment.is_some() {
            header.push_str(
                "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=mix-r13\r\n\r\n",
            );
        } else {
            header.push_str(str::from_utf8(PLAIN_MIME).unwrap());
        }
        let text = format!("合成邮件 {label} {identity} {uid}\r\n缓存搜索 中文关键字 synthetic workload {label}\r\n").into_bytes();
        Self {
            header: header.into_bytes(),
            text,
            html,
            attachment,
        }
    }

    pub fn structure(&self) -> String {
        let text = |subtype, bytes| {
            format!(
                "(\"TEXT\" \"{subtype}\" (\"CHARSET\" \"utf-8\") NIL NIL \"8BIT\" {bytes} 2 NIL NIL)"
            )
        };
        let plain = text("PLAIN", self.text.len());
        let Some(attachment) = self.attachment else {
            return plain;
        };
        let inline = self.html.as_ref().map_or(plain.clone(), |html| {
            format!(
                "({plain}{} \"ALTERNATIVE\" (\"BOUNDARY\" \"alt-r13\") NIL)",
                text("HTML", html.len())
            )
        });
        format!(
            "({inline}(\"APPLICATION\" \"OCTET-STREAM\" NIL NIL NIL \"BINARY\" {} NIL (\"ATTACHMENT\" (\"FILENAME\" \"sample.bin\"))) \"MIXED\" (\"BOUNDARY\" \"mix-r13\") NIL)",
            attachment.bytes
        )
    }

    fn body(&self) -> Vec<Piece<'_>> {
        use Piece::{Attachment, Bytes};
        let Some(attachment) = self.attachment else {
            return vec![Bytes(&self.text)];
        };
        let mut pieces = vec![Bytes(b"--mix-r13\r\n")];
        if let Some(html) = &self.html {
            pieces.extend([
                Bytes(ALTERNATIVE_MIME),
                Bytes(b"--alt-r13\r\n"),
                Bytes(PLAIN_MIME),
                Bytes(&self.text),
                Bytes(b"\r\n--alt-r13\r\n"),
                Bytes(HTML_MIME),
                Bytes(html),
                Bytes(b"\r\n--alt-r13--\r\n"),
            ]);
        } else {
            pieces.extend([Bytes(PLAIN_MIME), Bytes(&self.text)]);
        }
        pieces.extend([
            Bytes(b"\r\n--mix-r13\r\n"),
            Bytes(BINARY_MIME),
            Attachment(attachment),
            Bytes(b"\r\n--mix-r13--\r\n"),
        ]);
        pieces
    }

    pub fn size(&self) -> usize {
        self.header.len() + self.body().into_iter().map(Piece::len).sum::<usize>()
    }

    pub fn section(&self, name: &str, offset: usize, count: usize) -> Option<Vec<u8>> {
        use Piece::{Attachment, Bytes};
        let pieces = match name {
            "HEADER" => vec![Bytes(&self.header)],
            "" => {
                let mut raw = vec![Bytes(&self.header)];
                raw.extend(self.body());
                raw
            }
            "TEXT" => self.body(),
            "1" if self.html.is_none() => vec![Bytes(&self.text)],
            "1.MIME" if self.html.is_some() => vec![Bytes(ALTERNATIVE_MIME)],
            "1.MIME" | "1.1.MIME" => vec![Bytes(PLAIN_MIME)],
            "1.1" => vec![Bytes(&self.text)],
            "1.2.MIME" => vec![Bytes(HTML_MIME)],
            "1.2" => vec![Bytes(self.html.as_deref()?)],
            "2.MIME" => vec![Bytes(BINARY_MIME)],
            "2" => vec![Attachment(self.attachment?)],
            _ => return None,
        };
        let available = pieces
            .iter()
            .map(|piece| piece.len())
            .sum::<usize>()
            .saturating_sub(offset);
        let count = count.min(available);
        // No command generated by this fixture needs an unbounded large literal.
        if count > 4 * 1024 * 1024 {
            return None;
        }
        let mut output = Vec::with_capacity(count);
        let mut skip = offset;
        for piece in pieces {
            if skip >= piece.len() {
                skip -= piece.len();
                continue;
            }
            let take = (piece.len() - skip).min(count - output.len());
            piece.write(&mut output, skip, take);
            skip = 0;
            if output.len() == count {
                break;
            }
        }
        Some(output)
    }
}

pub(super) fn small(uid: u32) -> Payload {
    Payload {
        bytes: 257,
        seed: uid as u8,
    }
}
