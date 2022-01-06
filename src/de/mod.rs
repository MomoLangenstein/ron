/// Deserialization module.
use std::{borrow::Cow, io, str};

use serde::de::{self, DeserializeSeed, Deserializer as SerdeError, Visitor};

use self::{id::IdDeserializer, tag::TagDeserializer};
pub use crate::error::{Error, Position, SpannedError};
use crate::{
    error::{Result, SpannedResult},
    extensions::Extensions,
    options::Options,
    parse::{AnyNum, Bytes, ParsedStr},
};

mod id;
mod tag;
#[cfg(test)]
mod tests;
mod value;

/// The RON deserializer.
///
/// If you just want to simply deserialize a value,
/// you can use the `from_str` convenience function.
pub struct Deserializer<'de> {
    bytes: Bytes<'de>,
    newtype_variant: bool,
    last_identifier: Option<&'de str>,
    any_newtype: bool,
}

impl<'de> Deserializer<'de> {
    // Cannot implement trait here since output is tied to input lifetime 'de.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(input: &'de str) -> SpannedResult<Self> {
        Self::from_str_with_options(input, Options::default())
    }

    pub fn from_bytes(input: &'de [u8]) -> SpannedResult<Self> {
        Self::from_bytes_with_options(input, Options::default())
    }

    pub fn from_str_with_options(input: &'de str, options: Options) -> SpannedResult<Self> {
        Self::from_bytes_with_options(input.as_bytes(), options)
    }

    pub fn from_bytes_with_options(input: &'de [u8], options: Options) -> SpannedResult<Self> {
        let mut deserializer = Deserializer {
            bytes: Bytes::new(input)?,
            newtype_variant: false,
            last_identifier: None,
            any_newtype: false,
        };

        deserializer.bytes.exts |= options.default_extensions;

        Ok(deserializer)
    }

    pub fn remainder(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(self.bytes.bytes())
    }

    pub fn span_error(&self, code: Error) -> SpannedError {
        self.bytes.span_error(code)
    }
}

/// A convenience function for building a deserializer
/// and deserializing a value of type `T` from a reader.
pub fn from_reader<R, T>(rdr: R) -> SpannedResult<T>
where
    R: io::Read,
    T: de::DeserializeOwned,
{
    Options::default().from_reader(rdr)
}

/// A convenience function for building a deserializer
/// and deserializing a value of type `T` from a string.
pub fn from_str<'a, T>(s: &'a str) -> SpannedResult<T>
where
    T: de::Deserialize<'a>,
{
    Options::default().from_str(s)
}

/// A convenience function for building a deserializer
/// and deserializing a value of type `T` from bytes.
pub fn from_bytes<'a, T>(s: &'a [u8]) -> SpannedResult<T>
where
    T: de::Deserialize<'a>,
{
    Options::default().from_bytes(s)
}

impl<'de> Deserializer<'de> {
    /// Check if the remaining bytes are whitespace only,
    /// otherwise return an error.
    pub fn end(&mut self) -> Result<()> {
        self.bytes.skip_ws()?;

        if self.bytes.bytes().is_empty() {
            Ok(())
        } else {
            Err(Error::TrailingCharacters)
        }
    }

    /// Called from `deserialize_any` when a struct was detected. Decides if
    /// there is a unit, tuple or usual struct and deserializes it
    /// accordingly.
    ///
    /// This method assumes there is no identifier left.
    fn handle_any_struct<V>(&mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        // Create a working copy
        let mut bytes = self.bytes;

        // The caller checks for a brace before calling this
        bytes.advance_single()?;

        bytes.skip_ws()?;

        let id = bytes.identifier();
        bytes.skip_ws()?;

        if id.is_ok() && bytes.peek() == Some(b':') {
            // first two arguments are technically incorrect, but ignored anyway
            self.deserialize_struct("", &[], visitor)
        } else if !self.any_newtype {
            // first argument is ignored
            self.deserialize_tuple(0, visitor)
        } else {
            self.any_newtype = true;

            let mut braces = 1;
            let mut comma = false;
            while braces > 0 {
                let c = bytes.eat_byte()?;
                if c == b'(' || c == b'[' || c == b'{' {
                    braces += 1;
                } else if c == b')' || c == b']' || c == b'}' {
                    braces -= 1;
                } else if c == b',' && braces == 1 {
                    comma = true;
                    break;
                }
            }

            if comma {
                // first argument is ignored
                self.deserialize_tuple(0, visitor)
            } else {
                self.bytes.consume("(");
                self.bytes.skip_ws()?;
                let res = self.deserialize_any(visitor);
                self.bytes.skip_ws()?;
                self.bytes.consume(")");

                res
            }
        }
    }

    /// Called from `deserialize_struct`, `struct_variant`, and `handle_any_struct`.
    /// Handles deserialising the enclosing parentheses and everything in between.
    ///
    /// This method assumes there is no struct name identifier left.
    fn handle_struct_after_name<V>(
        &mut self,
        name_for_pretty_errors_only: &'static str,
        visitor: V,
    ) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if self.newtype_variant || self.bytes.consume("(") {
            let old_newtype_variant = self.newtype_variant;
            self.newtype_variant = false;

            let value = visitor
                .visit_map(CommaSeparated::new(b')', self))
                .map_err(|err| {
                    struct_error_name(
                        err,
                        if !old_newtype_variant && !name_for_pretty_errors_only.is_empty() {
                            Some(name_for_pretty_errors_only)
                        } else {
                            None
                        },
                    )
                })?;

            self.bytes.comma()?;

            if old_newtype_variant || self.bytes.consume(")") {
                Ok(value)
            } else {
                Err(Error::ExpectedStructLikeEnd)
            }
        } else if name_for_pretty_errors_only.is_empty() {
            Err(Error::ExpectedStructLike)
        } else {
            Err(Error::ExpectedNamedStructLike(name_for_pretty_errors_only))
        }
    }
}

struct SingletonMap<'a, 'de: 'a> {
    de: &'a mut Deserializer<'de>,
    key: Option<&'a [u8]>,
}

impl<'de, 'a> de::MapAccess<'de> for SingletonMap<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: DeserializeSeed<'de>,
    {
        use serde::de::{value::StrDeserializer, IntoDeserializer};

        let res = self
            .key
            .map(|key| {
                let deserializer: StrDeserializer<Error> =
                    std::str::from_utf8(key)?.into_deserializer();
                seed.deserialize(deserializer)
            })
            .transpose();

        self.key = None;
        res
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: DeserializeSeed<'de>,
    {
        self.de.bytes.skip_ws()?;

        let old_any_newtype = self.de.any_newtype;
        self.de.any_newtype = true;
        let res = seed.deserialize(&mut *self.de);
        self.de.any_newtype = old_any_newtype;

        res
    }
}

impl<'de, 'a> de::Deserializer<'de> for &'a mut Deserializer<'de> {
    type Error = Error;

    fn deserialize_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        // Newtype variants can only be unwrapped if we receive information
        //  about the wrapped type - with `deserialize_any` we don't
        self.newtype_variant = false;

        if self.bytes.consume_ident("true") {
            return visitor.visit_bool(true);
        } else if self.bytes.consume_ident("false") {
            return visitor.visit_bool(false);
        } else if self.bytes.check_ident("Some") {
            return self.deserialize_option(visitor);
        } else if self.bytes.consume_ident("None") {
            return visitor.visit_none();
        } else if self.bytes.consume("()") {
            return visitor.visit_unit();
        } else if self.bytes.consume_ident("inf") {
            return visitor.visit_f64(std::f64::INFINITY);
        } else if self.bytes.consume_ident("-inf") {
            return visitor.visit_f64(std::f64::NEG_INFINITY);
        } else if self.bytes.consume_ident("NaN") {
            return visitor.visit_f64(std::f64::NAN);
        }

        // `identifier` does not change state if it fails
        let ident = self.bytes.identifier().ok();

        if let Some(ident) = ident {
            self.bytes.skip_ws()?;

            return if self.bytes.peek() == Some(b'(') {
                visitor.visit_map(SingletonMap {
                    de: self,
                    key: Some(ident),
                })
            } else {
                visitor.visit_str(std::str::from_utf8(ident)?)
            };
        }

        match self.bytes.peek_or_eof()? {
            b'(' => self.handle_any_struct(visitor),
            b'[' => self.deserialize_seq(visitor),
            b'{' => self.deserialize_map(visitor),
            b'0'..=b'9' | b'+' | b'-' => {
                let any_num: AnyNum = self.bytes.any_num()?;

                match any_num {
                    AnyNum::F32(x) => visitor.visit_f32(x),
                    AnyNum::F64(x) => visitor.visit_f64(x),
                    AnyNum::I8(x) => visitor.visit_i8(x),
                    AnyNum::U8(x) => visitor.visit_u8(x),
                    AnyNum::I16(x) => visitor.visit_i16(x),
                    AnyNum::U16(x) => visitor.visit_u16(x),
                    AnyNum::I32(x) => visitor.visit_i32(x),
                    AnyNum::U32(x) => visitor.visit_u32(x),
                    AnyNum::I64(x) => visitor.visit_i64(x),
                    AnyNum::U64(x) => visitor.visit_u64(x),
                    #[cfg(feature = "integer128")]
                    AnyNum::I128(x) => visitor.visit_i128(x),
                    #[cfg(feature = "integer128")]
                    AnyNum::U128(x) => visitor.visit_u128(x),
                }
            }
            b'.' => self.deserialize_f64(visitor),
            b'"' | b'r' => self.deserialize_string(visitor),
            b'\'' => self.deserialize_char(visitor),
            other => Err(Error::UnexpectedByte(other as char)),
        }
    }

    fn deserialize_bool<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_bool(self.bytes.bool()?)
    }

    fn deserialize_i8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i8(self.bytes.signed_integer()?)
    }

    fn deserialize_i16<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i16(self.bytes.signed_integer()?)
    }

    fn deserialize_i32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i32(self.bytes.signed_integer()?)
    }

    fn deserialize_i64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i64(self.bytes.signed_integer()?)
    }

    #[cfg(feature = "integer128")]
    fn deserialize_i128<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_i128(self.bytes.signed_integer()?)
    }

    fn deserialize_u8<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u8(self.bytes.unsigned_integer()?)
    }

    fn deserialize_u16<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u16(self.bytes.unsigned_integer()?)
    }

    fn deserialize_u32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u32(self.bytes.unsigned_integer()?)
    }

    fn deserialize_u64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u64(self.bytes.unsigned_integer()?)
    }

    #[cfg(feature = "integer128")]
    fn deserialize_u128<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_u128(self.bytes.unsigned_integer()?)
    }

    fn deserialize_f32<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f32(self.bytes.float()?)
    }

    fn deserialize_f64<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_f64(self.bytes.float()?)
    }

    fn deserialize_char<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        visitor.visit_char(self.bytes.char()?)
    }

    fn deserialize_str<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        match self.bytes.string()? {
            ParsedStr::Allocated(s) => visitor.visit_string(s),
            ParsedStr::Slice(s) => visitor.visit_borrowed_str(s),
        }
    }

    fn deserialize_string<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.deserialize_str(visitor)
    }

    fn deserialize_bytes<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.deserialize_byte_buf(visitor)
    }

    fn deserialize_byte_buf<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        let res = {
            let string = self.bytes.string()?;
            let base64_str = match string {
                ParsedStr::Allocated(ref s) => s.as_str(),
                ParsedStr::Slice(s) => s,
            };
            base64::decode(base64_str)
        };

        match res {
            Ok(byte_buf) => visitor.visit_byte_buf(byte_buf),
            Err(err) => Err(Error::Base64Error(err)),
        }
    }

    fn deserialize_option<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if self.bytes.consume("None") {
            visitor.visit_none()
        } else if self.bytes.consume("Some") && {
            self.bytes.skip_ws()?;
            self.bytes.consume("(")
        } {
            self.bytes.skip_ws()?;

            let v = visitor.visit_some(&mut *self)?;

            self.bytes.skip_ws()?;

            if self.bytes.consume(")") {
                Ok(v)
            } else {
                Err(Error::ExpectedOptionEnd)
            }
        } else if self.bytes.exts.contains(Extensions::IMPLICIT_SOME) {
            visitor.visit_some(&mut *self)
        } else {
            Err(Error::ExpectedOption)
        }
    }

    // In Serde, unit means an anonymous value containing no data.
    fn deserialize_unit<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if self.newtype_variant || self.bytes.consume("()") {
            self.newtype_variant = false;

            visitor.visit_unit()
        } else {
            Err(Error::ExpectedUnit)
        }
    }

    fn deserialize_unit_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if self.newtype_variant || self.bytes.consume_struct_name(name)? {
            self.newtype_variant = false;

            visitor.visit_unit()
        } else {
            self.deserialize_unit(visitor)
        }
    }

    fn deserialize_newtype_struct<V>(self, name: &'static str, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if name == crate::value::raw::RAW_VALUE_TOKEN {
            let bytes_before = self.bytes.bytes();
            self.bytes.skip_ws()?;
            let _ignored = self.deserialize_ignored_any(serde::de::IgnoredAny)?;
            self.bytes.skip_ws()?;
            let bytes_after = self.bytes.bytes();

            let ron_bytes = &bytes_before[..bytes_before.len() - bytes_after.len()];
            let ron_str = str::from_utf8(ron_bytes).map_err(Error::from)?;

            return visitor
                .visit_borrowed_str::<Error>(ron_str)
                .map_err(|_| Error::ExpectedRawValue);
        }

        if self.bytes.exts.contains(Extensions::UNWRAP_NEWTYPES) || self.newtype_variant {
            self.newtype_variant = false;

            return visitor.visit_newtype_struct(&mut *self);
        }

        self.bytes.consume_struct_name(name)?;

        self.bytes.skip_ws()?;

        if self.bytes.consume("(") {
            self.bytes.skip_ws()?;
            let value = visitor.visit_newtype_struct(&mut *self)?;
            self.bytes.comma()?;

            if self.bytes.consume(")") {
                Ok(value)
            } else {
                Err(Error::ExpectedStructLikeEnd)
            }
        } else if name.is_empty() {
            Err(Error::ExpectedStructLike)
        } else {
            Err(Error::ExpectedNamedStructLike(name))
        }
    }

    fn deserialize_seq<V>(mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.newtype_variant = false;

        if self.bytes.consume("[") {
            let value = visitor.visit_seq(CommaSeparated::new(b']', self))?;
            self.bytes.comma()?;

            if self.bytes.consume("]") {
                Ok(value)
            } else {
                Err(Error::ExpectedArrayEnd)
            }
        } else {
            Err(Error::ExpectedArray)
        }
    }

    fn deserialize_tuple<V>(mut self, _len: usize, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if self.newtype_variant || self.bytes.consume("(") {
            let old_newtype_variant = self.newtype_variant;
            self.newtype_variant = false;

            let value = visitor.visit_seq(CommaSeparated::new(b')', self))?;
            self.bytes.comma()?;

            if old_newtype_variant || self.bytes.consume(")") {
                Ok(value)
            } else {
                Err(Error::ExpectedStructLikeEnd)
            }
        } else {
            Err(Error::ExpectedStructLike)
        }
    }

    fn deserialize_tuple_struct<V>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if !self.newtype_variant {
            self.bytes.consume_struct_name(name)?;
        }

        self.deserialize_tuple(len, visitor).map_err(|e| match e {
            Error::ExpectedStructLike if !name.is_empty() => Error::ExpectedNamedStructLike(name),
            e => e,
        })
    }

    fn deserialize_map<V>(mut self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.newtype_variant = false;

        if self.bytes.consume("{") {
            let value = visitor.visit_map(CommaSeparated::new(b'}', self))?;
            self.bytes.comma()?;

            if self.bytes.consume("}") {
                Ok(value)
            } else {
                Err(Error::ExpectedMapEnd)
            }
        } else {
            Err(Error::ExpectedMap)
        }
    }

    fn deserialize_struct<V>(
        self,
        name: &'static str,
        _fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        if !self.newtype_variant {
            self.bytes.consume_struct_name(name)?;
        }

        self.bytes.skip_ws()?;

        self.handle_struct_after_name(name, visitor)
    }

    fn deserialize_enum<V>(
        self,
        name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.newtype_variant = false;

        match visitor.visit_enum(Enum::new(self)) {
            Ok(value) => Ok(value),
            Err(Error::NoSuchEnumVariant {
                expected,
                found,
                outer: None,
            }) if !name.is_empty() => Err(Error::NoSuchEnumVariant {
                expected,
                found,
                outer: Some(String::from(name)),
            }),
            Err(e) => Err(e),
        }
    }

    fn deserialize_identifier<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        let identifier = str::from_utf8(self.bytes.identifier()?).map_err(Error::from)?;

        self.last_identifier = Some(identifier);

        visitor.visit_str(identifier)
    }

    fn deserialize_ignored_any<V>(self, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.deserialize_any(visitor)
    }
}

struct CommaSeparated<'a, 'de: 'a> {
    de: &'a mut Deserializer<'de>,
    terminator: u8,
    had_comma: bool,
}

impl<'a, 'de> CommaSeparated<'a, 'de> {
    fn new(terminator: u8, de: &'a mut Deserializer<'de>) -> Self {
        CommaSeparated {
            de,
            terminator,
            had_comma: true,
        }
    }

    fn has_element(&mut self) -> Result<bool> {
        self.de.bytes.skip_ws()?;

        match (
            self.had_comma,
            self.de.bytes.peek_or_eof()? != self.terminator,
        ) {
            // Trailing comma, maybe has a next element
            (true, has_element) => Ok(has_element),
            // No trailing comma but terminator
            (false, false) => Ok(false),
            // No trailing comma or terminator
            (false, true) => Err(Error::ExpectedComma),
        }
    }
}

impl<'de, 'a> de::SeqAccess<'de> for CommaSeparated<'a, 'de> {
    type Error = Error;

    fn next_element_seed<T>(&mut self, seed: T) -> Result<Option<T::Value>>
    where
        T: DeserializeSeed<'de>,
    {
        if self.has_element()? {
            let res = seed.deserialize(&mut *self.de)?;

            self.had_comma = self.de.bytes.comma()?;

            Ok(Some(res))
        } else {
            Ok(None)
        }
    }
}

impl<'de, 'a> de::MapAccess<'de> for CommaSeparated<'a, 'de> {
    type Error = Error;

    fn next_key_seed<K>(&mut self, seed: K) -> Result<Option<K::Value>>
    where
        K: DeserializeSeed<'de>,
    {
        if self.has_element()? {
            if self.terminator == b')' {
                seed.deserialize(&mut IdDeserializer::new(&mut *self.de))
                    .map(Some)
            } else {
                seed.deserialize(&mut *self.de).map(Some)
            }
        } else {
            Ok(None)
        }
    }

    fn next_value_seed<V>(&mut self, seed: V) -> Result<V::Value>
    where
        V: DeserializeSeed<'de>,
    {
        self.de.bytes.skip_ws()?;

        if self.de.bytes.consume(":") {
            self.de.bytes.skip_ws()?;

            let res = seed.deserialize(&mut TagDeserializer::new(&mut *self.de))?;

            self.had_comma = self.de.bytes.comma()?;

            Ok(res)
        } else {
            Err(Error::ExpectedMapColon)
        }
    }
}

struct Enum<'a, 'de: 'a> {
    de: &'a mut Deserializer<'de>,
}

impl<'a, 'de> Enum<'a, 'de> {
    fn new(de: &'a mut Deserializer<'de>) -> Self {
        Enum { de }
    }
}

impl<'de, 'a> de::EnumAccess<'de> for Enum<'a, 'de> {
    type Error = Error;
    type Variant = Self;

    fn variant_seed<V>(self, seed: V) -> Result<(V::Value, Self::Variant)>
    where
        V: DeserializeSeed<'de>,
    {
        self.de.bytes.skip_ws()?;

        let value = seed.deserialize(&mut *self.de)?;

        Ok((value, self))
    }
}

impl<'de, 'a> de::VariantAccess<'de> for Enum<'a, 'de> {
    type Error = Error;

    fn unit_variant(self) -> Result<()> {
        Ok(())
    }

    fn newtype_variant_seed<T>(self, seed: T) -> Result<T::Value>
    where
        T: DeserializeSeed<'de>,
    {
        let newtype_variant = self.de.last_identifier;

        self.de.bytes.skip_ws()?;

        if self.de.bytes.consume("(") {
            self.de.bytes.skip_ws()?;

            self.de.newtype_variant = self
                .de
                .bytes
                .exts
                .contains(Extensions::UNWRAP_VARIANT_NEWTYPES);

            let val = seed
                .deserialize(&mut *self.de)
                .map_err(|err| struct_error_name(err, newtype_variant))?;

            self.de.newtype_variant = false;

            self.de.bytes.comma()?;

            if self.de.bytes.consume(")") {
                Ok(val)
            } else {
                Err(Error::ExpectedStructLikeEnd)
            }
        } else {
            Err(Error::ExpectedStructLike)
        }
    }

    fn tuple_variant<V>(self, len: usize, visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        self.de.bytes.skip_ws()?;

        self.de.deserialize_tuple(len, visitor)
    }

    fn struct_variant<V>(self, _fields: &'static [&'static str], visitor: V) -> Result<V::Value>
    where
        V: Visitor<'de>,
    {
        let struct_variant = self.de.last_identifier;

        self.de.bytes.skip_ws()?;

        self.de
            .handle_struct_after_name("", visitor)
            .map_err(|err| struct_error_name(err, struct_variant))
    }
}

fn struct_error_name(error: Error, name: Option<&str>) -> Error {
    match error {
        Error::NoSuchStructField {
            expected,
            found,
            outer: None,
        } => Error::NoSuchStructField {
            expected,
            found,
            outer: name.map(ToOwned::to_owned),
        },
        Error::MissingStructField { field, outer: None } => Error::MissingStructField {
            field,
            outer: name.map(ToOwned::to_owned),
        },
        Error::DuplicateStructField { field, outer: None } => Error::DuplicateStructField {
            field,
            outer: name.map(ToOwned::to_owned),
        },
        e => e,
    }
}
