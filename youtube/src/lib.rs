use async_trait::async_trait;
use phosphorus_core::plugin_manager::{
    downloader::*,
    error::{ParseError, PluginError},
    plugin::Plugin,
    query::*,
    streamer::{StreamBuilder, Streamer},
};
use regex::Regex;
use serde_json::Value;
use std::{
    error::Error,
    process::{Child, Command, Stdio},
    time::Duration,
};

const BASE_URL: &'static str = "https://youtube.com/results";
const SOURCE_URL: &'static str = "https://youtube.com/watch";
const USER_AGENT: &'static str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/104.0.5112.102 Safari/537.36";

/// Plugin that allows to query YouTube and
/// retrieve download-usefull information
pub struct YouTube {}

#[async_trait]
impl Plugin for YouTube {
    fn method(&self) -> reqwest::Method {
        reqwest::Method::GET
    }

    fn base_url(&self) -> &'static str {
        BASE_URL
    }

    fn query(
        &self,
        info: &QueryInfo,
        mut req_builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::Request, Box<dyn Error>> {
        req_builder = req_builder.query(&[("search_query", info.raw())]);
        req_builder = req_builder.header("user-agent", USER_AGENT);
        Ok(req_builder.build()?)
    }

    async fn parse(
        &self,
        _info: &QueryInfo,
        resp: reqwest::Response,
    ) -> Result<QueryResult, Box<dyn Error>> {
        let text = resp.text().await?;
        let json = get_json(&text)?;

        let contents = json.get("contents").unwrap().as_array().unwrap();
        let mut items = vec![];
        for token in contents {
            let raw = token.read("videoRenderer");
            if raw.is_ok() {
                let item = QueryResultData::parse(raw.unwrap())?;
                items.push(item);
            }
        }

        Ok(QueryResult::new(items))
    }

    fn download(&self) -> Downloader {
        DownloadBuilder::new(download_fn)
    }

    fn stream(&self) -> Streamer {
        StreamBuilder::new(stream_fn)
    }
}

/// Creates a sub-process that downloads the media associated to `url`. That media is than
/// saved as `file_name.[ext]`. The handler to the sub-process is returned.
pub fn download_fn(url: &str, file_name: &str) -> Result<Child, Box<dyn Error>> {
    let mut download_command = Command::new("yt-dlp");
    download_command.args(["--extract-audio", "--audio-format", "mp3"]);
    let output_file = format!("{}.%(ext)s", file_name);
    download_command.args(["-o", &output_file]);
    download_command.arg(url);

    let handler = download_command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(handler)
}

pub fn stream_fn(url: &str, file_name: &str) -> Result<std::process::Child, Box<dyn Error>> {
    let mut url_getter = Command::new("youtube-dl");
    url_getter.arg("-g");
    url_getter.arg(url);
    let urls = url_getter.output()?;
    let urls = std::str::from_utf8(&urls.stdout).unwrap();

    let regex = Regex::new(r#"(https://.*)\s*$"#)?;
    let matches = regex.captures(urls);

    let url = if let Some(matches) = matches {
        if let Some(text) = matches.get(0) {
            text.as_str().trim()
        } else {
            return Err(Box::new(PluginError::StreamError(
                "No download url found for the audio stream".into(),
            )));
        }
    } else {
        return Err(Box::new(PluginError::StreamError(
            "No download url found".into(),
        )));
    };

    let mut stream_getter = Command::new("ffmpeg");
    stream_getter.args(["-i", url]);
    stream_getter.args(["-c:a", "libmp3lame"]);
    let file = format!("file:{}", file_name);
    stream_getter.arg(&file);
    stream_getter.arg("-y"); // If `file_name` exists, it's overwritten

    let handler = stream_getter
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(handler)
}

/// Given a json string, returns the associated `serde_json::Value`
/// instance.
fn get_json(text: &str) -> Result<Value, Box<dyn Error>> {
    let regex = Regex::new(r#"itemSectionRenderer":(\{"contents")"#).unwrap();
    let begin_range = match regex.captures(&text) {
        Some(groups) => match groups.get(1) {
            Some(group) => group.range(),
            None => {
                return Err(Box::new(ParseError::ParsableTextNotFound));
            }
        },
        None => {
            return Err(Box::new(ParseError::ParsableTextNotFound));
        }
    };

    let regex =
        Regex::new(r#"\],"trackingParams":"[a-zA-Z0-9=_-]*"(\})\},\{"continuationItemRenderer""#)
            .unwrap();
    let end_range = match regex.captures(&text) {
        Some(groups) => match groups.get(1) {
            Some(group) => group.range(),
            None => {
                return Err(Box::new(ParseError::ParsableTextNotFound));
            }
        },
        None => {
            return Err(Box::new(ParseError::ParsableTextNotFound));
        }
    };

    if begin_range.start >= end_range.end {
        return Err(Box::new(ParseError::InvalidResponseText));
    }

    match serde_json::from_str(&text[begin_range.start..end_range.end]) {
        Ok(value) => Ok(value),
        Err(cause) => Err(Box::new(ParseError::JsonUnparsable(cause.to_string()))),
    }
}

impl Deserializable<Value, QueryResultData, YouTube> for QueryResultData {
    fn parse(source: &Value) -> Result<Self, Box<dyn Error>> {
        let artist_thumbnail = source
            .read("channelThumbnailSupportedRenderers")?
            .read("channelThumbnailWithLinkRenderer")?
            .read("thumbnail")?
            .read("thumbnails")?
            .as_array()
            .unwrap()[0]
            .read("url")?
            .as_str()
            .unwrap();
        let artist_thumbnail = reqwest::Url::parse(artist_thumbnail)?;
        let artist_name = source
            .read("longBylineText")?
            .read("runs")?
            .as_array()
            .unwrap()[0]
            .read("text")?
            .as_str()
            .unwrap();

        // Reads duration an converts it to a `Duration` instance
        let duration = source
            .read("lengthText")?
            .read("simpleText")?
            .as_str()
            .unwrap();
        let duration_tokens: Vec<u64> = duration
            .split(":")
            .map(|token| u64::from_str_radix(token, 10).unwrap())
            .collect();

        let mut i = duration_tokens.len();
        let mut total_duration = 0;
        for token in duration_tokens {
            i -= 1;
            total_duration += token * 60_u64.pow(i as u32);
        }

        let duration = Duration::from_secs(total_duration);

        let track_id = source.read("videoId")?.as_str().unwrap();
        let track_url = reqwest::Url::parse(&format!("{}?v={}", SOURCE_URL, track_id)).unwrap();

        let track_name = source.read("title")?.read("runs")?.as_array().unwrap()[0]
            .read("text")?
            .as_str()
            .unwrap();
        let track_thumbnail = source
            .read("thumbnail")?
            .read("thumbnails")?
            .as_array()
            .unwrap()[0]
            .read("url")?
            .as_str()
            .unwrap();
        let track_thumbnail = reqwest::Url::parse(track_thumbnail)?;

        Ok(QueryResultData::new(
            track_id,
            track_name,
            track_url,
            track_thumbnail,
            artist_name,
            artist_thumbnail,
            duration,
        ))
    }
}

trait YTItemParser {
    /// Utility to retrieve a `Value` from a map representing a JSON
    fn read(&self, field_name: &str) -> Result<&Value, Box<dyn Error>>;
}

impl YTItemParser for Value {
    fn read(&self, field_name: &str) -> Result<&Value, Box<dyn Error>> {
        match self.get(field_name) {
            Some(value) => Ok(value),
            None => {
                return Err(Box::new(ParseError::JsonUnparsable(format!(
                    "Missing field `{}`",
                    field_name
                ))));
            }
        }
    }
}

#[cfg(test)]
mod test {
    use regex::Regex;

    /// This function checks is the regex used to retrieve the starting point
    /// for data in raw response is still valid.
    #[test]
    fn regex_extraction() {
        let regex = Regex::new(r#"itemSectionRenderer[\S|\s]*(\{\s*"contents)"#).unwrap();
        let text = r#"ntents":[{"itemSectionRenderer":{"contents":[{"#;
        let group = regex.captures(&text).unwrap().get(1).unwrap();

        assert_eq!(
            group.as_str(),
            r#"{"contents"#,
            "Testing for the content of the matched group"
        );
        assert_eq!(
            (group.start(), group.end()),
            (32, 42),
            "Testing for the range of the matched group"
        );
    }
}
