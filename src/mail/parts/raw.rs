use super::{mapping::Section, *};

pub(in crate::mail) struct RawManifest<'a, 'b> {
    pub manifest: PartManifest,
    pub entities: Vec<&'a mime::Part<'b>>,
}

fn visit<'a, 'b>(
    part: &'a mime::Part<'b>,
    section: Section,
    depth: usize,
    index: &mut usize,
    emit: &mut impl FnMut(Section, usize, &'a mime::Part<'b>),
) -> Result<()> {
    if depth > mime::MAX_DEPTH || *index >= mime::MAX_PARTS {
        return Err(MailError::limit_kind(LimitKind::MimeStructure));
    }
    if part.subparts.is_empty() || part.is_attachment() {
        emit(section, *index, part);
        *index += 1;
    } else {
        for (position, child) in part.subparts.iter().enumerate() {
            visit(
                child,
                section
                    .child(position)
                    .map_err(|_| MailError::limit_kind(LimitKind::MimeStructure))?,
                depth + 1,
                index,
                emit,
            )?;
        }
    }
    Ok(())
}

fn root(part: &mime::Part<'_>) -> Section {
    Section(if part.ctype.mimetype.starts_with("multipart/") {
        vec![]
    } else {
        vec![1]
    })
}

#[cfg(test)]
pub(in crate::mail) fn raw_leaves<'a, 'b>(
    part: &'a mime::Part<'b>,
    out: &mut Vec<&'a mime::Part<'b>>,
    depth: usize,
) -> Result<()> {
    let mut index = out.len();
    visit(part, root(part), depth, &mut index, &mut |_, _, part| {
        out.push(part)
    })
}

pub(in crate::mail) fn from_raw<'a, 'b>(part: &'a mime::Part<'b>) -> Result<RawManifest<'a, 'b>> {
    let mut result = RawManifest {
        manifest: PartManifest { parts: vec![] },
        entities: vec![],
    };
    let mut attachments = 0;
    visit(part, root(part), 0, &mut 0, &mut |section, index, part| {
        let attachment = part.is_attachment();
        let ordinal = attachment.then(|| {
            let value = attachments;
            attachments += 1;
            value
        });
        let disposition = match part.get_content_disposition().disposition {
            mailparse::DispositionType::Inline => Some("inline".into()),
            mailparse::DispositionType::Attachment => Some("attachment".into()),
            mailparse::DispositionType::FormData => Some("form-data".into()),
            mailparse::DispositionType::Extension(value) if !value.is_empty() => Some(value),
            _ => None,
        };
        result.manifest.parts.push(ManifestPart {
            section,
            attachment_index: index,
            attachment_ordinal: ordinal,
            is_attachment: attachment,
            content_type: part.ctype.mimetype.clone(),
            transfer_encoding: part
                .headers
                .get_first_value("Content-Transfer-Encoding")
                .map(|v| v.trim().to_ascii_lowercase())
                .unwrap_or_else(|| "7bit".into()),
            encoded_size: u32::try_from(part.encoded_len()).ok(),
            disposition,
            filename: part.filename(),
            content_id: part.headers.get_first_value("Content-ID"),
            downloadable: part.attachment_encoding_supported(),
        });
        result.entities.push(part);
    })?;
    Ok(result)
}
