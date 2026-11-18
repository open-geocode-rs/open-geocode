//! `languageCode`, the one language a Google request names results in.

use axum::http::HeaderMap;

use super::invalid;
use crate::{
    api::Languages,
    compat::{VendorError, header},
    pack::{Reader, RecordId},
};

/// The language asked, and the Pack that tells which names og holds in it.
/// og names a record it has no name for in that language by its local name,
/// whose language og does not know, so a field saying which language a name
/// is in is set only for a name og holds in the language asked.
pub(super) struct Language<'a> {
    pack: &'a Reader,
    code: Option<String>,
}

impl<'a> Language<'a> {
    /// `languageCode`, one BCP-47 code. A list, or anything else that is not
    /// one code, is Google's `INVALID_ARGUMENT`. None asked is og's default,
    /// local names.
    pub(super) fn parse(pack: &'a Reader, code: Option<&str>) -> Result<Self, VendorError> {
        let code = code.map(str::trim).filter(|code| !code.is_empty());
        if let Some(code) = code
            && !is_one_code(code)
        {
            return Err(invalid(format!(
                "languageCode {code:?} is not a language code, such as de or zh-Hant"
            )));
        }
        Ok(Self {
            pack,
            code: code.map(str::to_string),
        })
    }

    /// The code to send og as `lang`.
    pub(super) fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    /// The language og names `record_id` in, as og holds it: `de` when
    /// `de-CH` was asked and og has only a `de` name. `None` when og names it
    /// by its local name.
    pub(super) fn of(&self, record_id: Option<RecordId>) -> Option<String> {
        let (held, _) = self
            .pack
            .language_name_as(record_id?, self.code.as_deref()?)?;
        Some(held.to_string())
    }
}

fn is_one_code(code: &str) -> bool {
    !code.contains(',') && Languages::parse(code).is_ok_and(|codes| codes.codes().len() == 1)
}

/// The first language of an `Accept-Language` header, such as `de-CH` of
/// `de-CH,de;q=0.9`. `None` for none, for `*`, and for a tag that is not a
/// language code, since a browser's header is no request to refuse.
pub(super) fn accept_language(headers: &HeaderMap) -> Option<String> {
    let first = header(headers, "accept-language")?.split(',').next()?;
    let tag = first.split(';').next()?.trim();
    is_one_code(tag).then(|| tag.to_string())
}
