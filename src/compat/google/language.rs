//! `languageCode`, the one language a Google request names results in.

use axum::http::HeaderMap;

use super::invalid;
use crate::{
    api::{Context, Languages},
    compat::{self, VendorError, header},
    pack::{Reader, RecordId},
};

/// The language asked, and the Pack that tells which names og holds in it.
/// og names a record it has no name for in that language by its local name,
/// which is in the language its place speaks ([`compat::place_language`]).
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

    /// The language og names `record_id` in. It is the language asked, as
    /// og holds it (`de` when `de-CH` was asked and og has only a `de`
    /// name), else the language of the local name, the one `place`
    /// speaks. `None` when neither is known.
    pub(super) fn of(&self, record_id: Option<RecordId>, place: &Context) -> Option<String> {
        self.held(record_id)
            .or_else(|| compat::place_language(self.pack, place))
    }

    /// The language local names are written in where `place` is.
    pub(super) fn local(&self, place: &Context) -> Option<String> {
        compat::place_language(self.pack, place)
    }

    pub(super) fn reference_point(&self, id: RecordId) -> Option<(f64, f64)> {
        let (_, lon, lat) = self.pack.records().point(id).ok()?;
        Some((lon, lat))
    }

    fn held(&self, record_id: Option<RecordId>) -> Option<String> {
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
