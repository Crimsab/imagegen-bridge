//! Common image-generation parameters and their wire representation.

use std::{fmt, str::FromStr};

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::{BridgeError, ErrorCode};

macro_rules! string_enum {
    ($(#[$meta:meta])* $visibility:vis enum $name:ident {
        $($(#[$variant_meta:meta])* $variant:ident => $wire:literal),+ $(,)?
    }) => {
        $(#[$meta])*
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
            Serialize, Deserialize, JsonSchema,
        )]
        #[serde(rename_all = "snake_case")]
        $visibility enum $name {
            $(
                $(#[$variant_meta])*
                #[doc = concat!("Wire value `", $wire, "`.")]
                #[serde(rename = $wire)]
                $variant
            ),+
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(match self { $(Self::$variant => $wire),+ })
            }
        }
    };
}

string_enum! {
    /// Requested generation quality.
    pub enum Quality {
        Auto => "auto",
        Low => "low",
        Medium => "medium",
        High => "high",
    }
}

impl Default for Quality {
    fn default() -> Self {
        Self::Auto
    }
}

string_enum! {
    /// Encoded output image format.
    pub enum OutputFormat {
        Png => "png",
        Jpeg => "jpeg",
        Webp => "webp",
    }
}

impl Default for OutputFormat {
    fn default() -> Self {
        Self::Png
    }
}

string_enum! {
    /// Requested background behavior.
    pub enum Background {
        Auto => "auto",
        Opaque => "opaque",
        Transparent => "transparent",
    }
}

impl Default for Background {
    fn default() -> Self {
        Self::Auto
    }
}

string_enum! {
    /// Provider moderation strictness where configurable.
    pub enum Moderation {
        Auto => "auto",
        Low => "low",
    }
}

impl Default for Moderation {
    fn default() -> Self {
        Self::Auto
    }
}

string_enum! {
    /// Desired output payload representation.
    pub enum ResponseFormat {
        B64Json => "b64_json",
        Url => "url",
        Artifact => "artifact",
        Metadata => "metadata",
    }
}

impl Default for ResponseFormat {
    fn default() -> Self {
        Self::B64Json
    }
}

string_enum! {
    /// Compatibility policy used during provider negotiation.
    pub enum CompatibilityMode {
        Strict => "strict",
        Normalize => "normalize",
        BestEffort => "best_effort",
    }
}

impl Default for CompatibilityMode {
    fn default() -> Self {
        Self::Strict
    }
}

string_enum! {
    /// Handling policy for a negative prompt.
    pub enum NegativePromptMode {
        Auto => "auto",
        Native => "native",
        Merge => "merge",
        Reject => "reject",
    }
}

impl Default for NegativePromptMode {
    fn default() -> Self {
        Self::Auto
    }
}

string_enum! {
    /// Visibility and requirement policy for an upstream revised prompt.
    pub enum RevisedPromptPolicy {
        Include => "include",
        Omit => "omit",
        Require => "require",
    }
}

impl Default for RevisedPromptPolicy {
    fn default() -> Self {
        Self::Include
    }
}

string_enum! {
    /// Codex conversation behavior for a request.
    pub enum SessionMode {
        Isolated => "isolated",
        Persistent => "persistent",
        Thread => "thread",
    }
}

impl Default for SessionMode {
    fn default() -> Self {
        Self::Isolated
    }
}

string_enum! {
    /// Coarse output resolution hint.
    pub enum Resolution {
        OneK => "1k",
        TwoK => "2k",
        FourK => "4k",
    }
}

/// Image size represented as `auto` or `WIDTHxHEIGHT`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageSize(String);

impl ImageSize {
    /// Automatic provider-selected size.
    pub const AUTO: &'static str = "auto";

    /// Constructs a validated explicit size.
    pub fn exact(width: u32, height: u32) -> Result<Self, BridgeError> {
        if width == 0 || height == 0 {
            return Err(BridgeError::new(
                ErrorCode::InvalidRequest,
                "image dimensions must be greater than zero",
            ));
        }
        Ok(Self(format!("{width}x{height}")))
    }

    /// Returns `None` for `auto`, otherwise the explicit dimensions.
    #[must_use]
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        if self.0 == Self::AUTO {
            return None;
        }
        let (width, height) = self.0.split_once('x')?;
        Some((width.parse().ok()?, height.parse().ok()?))
    }

    /// Returns true when the provider should choose the size.
    #[must_use]
    pub fn is_auto(&self) -> bool {
        self.0 == Self::AUTO
    }

    /// Returns the stable wire value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ImageSize {
    fn default() -> Self {
        Self(Self::AUTO.to_owned())
    }
}

impl fmt::Display for ImageSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ImageSize {
    type Err = BridgeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value == Self::AUTO {
            return Ok(Self::default());
        }
        let (width, height) = value.split_once('x').ok_or_else(|| {
            BridgeError::new(
                ErrorCode::InvalidRequest,
                "size must be `auto` or `WIDTHxHEIGHT`",
            )
        })?;
        if width.is_empty()
            || height.is_empty()
            || (width.len() > 1 && width.starts_with('0'))
            || (height.len() > 1 && height.starts_with('0'))
            || !width.bytes().all(|byte| byte.is_ascii_digit())
            || !height.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(BridgeError::new(
                ErrorCode::InvalidRequest,
                "size must be `auto` or `WIDTHxHEIGHT`",
            ));
        }
        Self::exact(
            width.parse().map_err(|_| {
                BridgeError::new(ErrorCode::InvalidRequest, "image width is out of range")
            })?,
            height.parse().map_err(|_| {
                BridgeError::new(ErrorCode::InvalidRequest, "image height is out of range")
            })?,
        )
    }
}

impl Serialize for ImageSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ImageSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value.parse().map_err(de::Error::custom)
    }
}

impl JsonSchema for ImageSize {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ImageSize".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^(auto|[1-9][0-9]*x[1-9][0-9]*)$",
            "examples": ["auto", "1024x1024", "1536x1024"]
        })
    }
}

/// Aspect ratio represented as `WIDTH:HEIGHT` with non-zero integer terms.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AspectRatio(String);

impl AspectRatio {
    /// Constructs a reduced aspect ratio.
    pub fn new(width: u32, height: u32) -> Result<Self, BridgeError> {
        if width == 0 || height == 0 {
            return Err(BridgeError::new(
                ErrorCode::InvalidRequest,
                "aspect ratio terms must be greater than zero",
            ));
        }
        let divisor = gcd(width, height);
        Ok(Self(format!("{}:{}", width / divisor, height / divisor)))
    }

    /// Returns the reduced integer ratio.
    #[must_use]
    pub fn terms(&self) -> (u32, u32) {
        let (width, height) = self.0.split_once(':').unwrap_or(("1", "1"));
        (width.parse().unwrap_or(1), height.parse().unwrap_or(1))
    }

    /// Returns the stable wire value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for AspectRatio {
    type Err = BridgeError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value.split_once(':').ok_or_else(|| {
            BridgeError::new(
                ErrorCode::InvalidRequest,
                "aspect_ratio must use `WIDTH:HEIGHT`",
            )
        })?;
        Self::new(
            width.parse().map_err(|_| {
                BridgeError::new(ErrorCode::InvalidRequest, "aspect ratio width is invalid")
            })?,
            height.parse().map_err(|_| {
                BridgeError::new(ErrorCode::InvalidRequest, "aspect ratio height is invalid")
            })?,
        )
    }
}

impl fmt::Display for AspectRatio {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AspectRatio {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for AspectRatio {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

impl JsonSchema for AspectRatio {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AspectRatio".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "pattern": "^[1-9][0-9]*:[1-9][0-9]*$",
            "examples": ["1:1", "3:2", "16:9"]
        })
    }
}

const fn gcd(mut left: u32, mut right: u32) -> u32 {
    while right != 0 {
        let remainder = left % right;
        left = right;
        right = remainder;
    }
    left
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn image_size_round_trips_as_a_string() {
        let size: ImageSize = "1536x1024".parse().unwrap();
        assert_eq!(size.dimensions(), Some((1536, 1024)));
        assert_eq!(serde_json::to_string(&size).unwrap(), "\"1536x1024\"");
        assert_eq!(
            serde_json::from_str::<ImageSize>("\"1536x1024\"").unwrap(),
            size
        );
    }

    #[test]
    fn image_size_rejects_ambiguous_values() {
        for value in ["", "1024", "0x1024", "1024X1024", "1x2x3", "01x1"] {
            assert!(value.parse::<ImageSize>().is_err(), "accepted {value}");
        }
    }

    #[test]
    fn aspect_ratio_is_reduced() {
        let ratio: AspectRatio = "1920:1080".parse().unwrap();
        assert_eq!(ratio.as_str(), "16:9");
        assert_eq!(ratio.terms(), (16, 9));
    }
}
