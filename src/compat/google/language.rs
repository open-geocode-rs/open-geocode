use axum::http::HeaderMap;

use super::invalid;
use crate::{
    api::{Context, Languages},
    compat::{self, VendorError, header},
    pack::{Reader, RecordId},
};

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

    pub(super) fn code(&self) -> Option<&str> {
        self.code.as_deref()
    }

    pub(super) fn of(&self, record_id: Option<RecordId>, place: &Context) -> Option<String> {
        self.held(record_id)
            .or_else(|| compat::place_language(self.pack, place))
    }

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

pub(super) fn accept_language(headers: &HeaderMap) -> Option<String> {
    let first = header(headers, "accept-language")?.split(',').next()?;
    let tag = first.split(';').next()?.trim();
    is_one_code(tag).then(|| tag.to_string())
}
