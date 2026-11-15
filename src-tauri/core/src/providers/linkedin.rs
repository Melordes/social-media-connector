//! LinkedIn: posts on the member's own profile, through the user's own
//! developer app (products "Sign In with LinkedIn using OpenID Connect" and
//! "Share on LinkedIn").
//!
//! Sign-in: OAuth 2.0 authorization code flow with the client secret. PKCE
//! is not used: LinkedIn only offers it on a separate endpoint that it has
//! to switch on for each app. Access tokens last 60 days; refresh tokens
//! exist only for approved partner apps, so most accounts have to be
//! reconnected every two months.
//!
//! OpenID Connect gives the member id (`sub`), name and picture but no public
//! handle, so the member's name doubles as the handle.
//!
//! Posting uses the versioned REST API (`/rest/posts`, `/rest/images`), which
//! needs a `LinkedIn-Version: YYYYMM` header. A version is published every
//! month (December is usually skipped) and switched off after about a year,
//! so the app uses the version from three months ago and tries neighbouring
//! months when LinkedIn answers 426 `NONEXISTENT_VERSION`.
//!
//! Reference (checked 2026-11-15): learn.microsoft.com/linkedin — 3-legged
//! OAuth flow, programmatic refresh tokens, Sign In with LinkedIn using
//! OpenID Connect, Posts API, Images API, MultiImage API, little text format,
//! versioning and error responses (Marketing version 202610).

use std::collections::HashMap;

use chrono::Datelike;
use reqwest::{RequestBuilder, Response, StatusCode};
use serde_json::{Value, json};

use super::{Ctx, OAuthBegin, OAuthStart, expect_code, expect_state, js, ju};
use crate::error::{PResult, ProviderError};
use crate::http;
use crate::media::PreparedImage;
use crate::model::*;

const API: &str = "https://api.linkedin.com";
const AUTH: &str = "https://www.linkedin.com";
const WEB: &str = "https://www.linkedin.com";
const SCOPES: &str = "openid profile w_member_social";
/// Use the API version from this many months ago.
const VERSION_LAG_MONTHS: i32 = 3;
/// Months (relative to the first choice) tried when a version is not active.
const VERSION_OFFSETS: [i32; 5] = [0, 1, -1, 2, -2];
/// Characters with a meaning in LinkedIn's `little` text format.
const RESERVED: [char; 15] = ['\\', '|', '{', '}', '@', '[', ']', '(', ')', '<', '>', '#', '*', '_', '~'];
/// `altText` limit of the Images and MultiImage APIs.
const ALT_TEXT_MAX: usize = 4086;
const EXPIRED: &str = "The LinkedIn sign-in has expired (LinkedIn sign-ins last 60 days). Reconnect the account.";

fn api(ctx: &Ctx) -> String {
    ctx.base("linkedin.api", API)
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.trim().is_empty())
}

// -------------------------------------------------------------- versions

fn month_string(index: i32) -> String {
    format!("{:04}{:02}", index.div_euclid(12), index.rem_euclid(12) + 1)
}

fn month_index(version: &str) -> Option<i32> {
    if version.len() != 6 || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let year: i32 = version[..4].parse().ok()?;
    let month: i32 = version[4..].parse().ok()?;
    (1..=12).contains(&month).then_some(year * 12 + month - 1)
}

/// The `LinkedIn-Version` to use on `date`: the month three months earlier.
pub(crate) fn api_version_for(date: chrono::NaiveDate) -> String {
    month_string(date.year() * 12 + date.month0() as i32 - VERSION_LAG_MONTHS)
}

/// `version` first, then the months around it.
fn version_candidates(version: &str) -> Vec<String> {
    match month_index(version) {
        Some(i) => VERSION_OFFSETS.iter().map(|d| month_string(i + d)).collect(),
        None => vec![version.to_string()],
    }
}

fn current_version() -> String {
    api_version_for(chrono::Utc::now().date_naive())
}

/// LinkedIn answers 426 `NONEXISTENT_VERSION` for versions that are switched
/// off (or not released yet).
fn version_not_active(status: StatusCode, body: &str) -> bool {
    status.as_u16() == 426
        || serde_json::from_str::<Value>(body).ok().and_then(|v| js(&v, "/code")).as_deref() == Some("NONEXISTENT_VERSION")
}

/// Sends a versioned REST request, moving to a neighbouring month if
/// LinkedIn says `version` is not active. `version` is updated to the one
/// that worked so later requests of the same task use it directly.
async fn versioned(creds: &Credentials, version: &mut String, build: impl Fn() -> RequestBuilder) -> PResult<Response> {
    for v in version_candidates(version) {
        let req = build()
            .bearer_auth(creds.token())
            .header("LinkedIn-Version", v.as_str())
            .header("X-Restli-Protocol-Version", "2.0.0");
        let resp = http::send(req).await?;
        let status = resp.status();
        if status.is_success() {
            *version = v;
            return Ok(resp);
        }
        let retry = http::retry_after_secs(&resp);
        let body = resp.text().await.unwrap_or_default();
        if version_not_active(status, &body) {
            log::info!("LinkedIn API version {v} is not active; trying another month");
            continue;
        }
        return Err(li_error(status, &body, retry));
    }
    Err(ProviderError::Other(format!(
        "LinkedIn did not accept API version {version} or the months around it. Check that this computer's date is correct."
    )))
}

// ------------------------------------------------------------------ http

fn li_error(status: StatusCode, body: &str, retry_after: Option<u64>) -> ProviderError {
    let detail = http::error_message(body).map(|d| format!(" ({d})")).unwrap_or_default();
    match status.as_u16() {
        401 => ProviderError::Unauthorized(format!(
            "LinkedIn did not accept the saved sign-in{detail}. LinkedIn sign-ins last 60 days; reconnect the account."
        )),
        403 => ProviderError::Forbidden(format!(
            "LinkedIn refused this action{detail}. Check that your LinkedIn developer app has the products \"Share on LinkedIn\" and \"Sign In with LinkedIn using OpenID Connect\"."
        )),
        _ => http::status_error(status, body, retry_after),
    }
}

async fn send(req: RequestBuilder) -> PResult<Response> {
    let resp = http::send(req).await?;
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let retry = http::retry_after_secs(&resp);
    let body = resp.text().await.unwrap_or_default();
    Err(li_error(status, &body, retry))
}

// --------------------------------------------------------------- sign-in

fn token_credentials(token: &Value, client_id: &str, client_secret: &str, previous_refresh: Option<&str>) -> PResult<Credentials> {
    let access = js(token, "/access_token").ok_or_else(|| ProviderError::parse("LinkedIn token", "no access_token"))?;
    let now = now_secs();
    let mut creds = Credentials {
        access_token: Some(access),
        // Only apps approved for programmatic refresh tokens get one.
        refresh_token: non_empty(js(token, "/refresh_token")).or_else(|| previous_refresh.map(str::to_string)),
        expires_at: ju(token, "/expires_in").map(|s| now + s as i64),
        client_id: Some(client_id.to_string()),
        client_secret: Some(client_secret.to_string()),
        extra: serde_json::Map::new(),
    };
    if let Some(s) = ju(token, "/refresh_token_expires_in") {
        creds.set_extra("refreshTokenExpiresAt", now + s as i64);
    }
    Ok(creds)
}

pub async fn begin(ctx: &Ctx, b: &OAuthBegin) -> PResult<OAuthStart> {
    let client_id = b.require_client_id()?;
    b.require_client_secret()?;
    let auth_url = crate::oauth::with_query(
        &format!("{}/oauth/v2/authorization", ctx.base("linkedin.auth", AUTH)),
        &[
            ("response_type", "code"),
            ("client_id", client_id),
            ("redirect_uri", &b.redirect_uri),
            ("state", &b.state),
            ("scope", SCOPES),
        ],
    )
    // Space-delimited scopes as %20, as in LinkedIn's examples.
    .replace('+', "%20");
    Ok(OAuthStart { auth_url, flow: json!({}) })
}

pub async fn finish(ctx: &Ctx, b: &OAuthBegin, _flow: &Value, params: &HashMap<String, String>) -> PResult<Vec<NewAccount>> {
    if matches!(params.get("error").map(String::as_str), Some("user_cancelled_login" | "user_cancelled_authorize")) {
        return Err(ProviderError::Forbidden("Access was declined in the browser.".into()));
    }
    expect_state(params, &b.state)?;
    let code = expect_code(params)?;
    let client_id = b.require_client_id()?;
    let client_secret = b.require_client_secret()?;
    let req = ctx
        .http
        .client
        .post(format!("{}/oauth/v2/accessToken", ctx.base("linkedin.auth", AUTH)))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &b.redirect_uri),
        ]);
    let token: Value = match http::send_json(req, "LinkedIn sign-in").await {
        Ok(v) => v,
        Err(ProviderError::Unauthorized(m)) => {
            return Err(ProviderError::invalid(format!(
                "LinkedIn did not accept the sign-in ({m}). Try again, and check the client ID and secret in Settings → Developer apps."
            )));
        }
        Err(e) => return Err(e),
    };
    let creds = token_credentials(&token, client_id, client_secret, None)?;
    let me = userinfo(ctx, &creds).await?;
    let sub = non_empty(js(&me, "/sub")).ok_or_else(|| ProviderError::parse("LinkedIn profile", "no member id"))?;
    let name = member_name(&me);
    Ok(vec![NewAccount {
        platform: Platform::Linkedin,
        server: String::new(),
        remote_id: sub,
        handle: name.clone(),
        display_name: name,
        avatar_url: non_empty(js(&me, "/picture")),
        profile_url: None,
        meta: json!({}),
        credentials: creds,
    }])
}

pub async fn refresh(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<Credentials> {
    let expired = || ProviderError::Unauthorized(EXPIRED.into());
    let refresh_token = creds.refresh_token.as_deref().filter(|s| !s.is_empty()).ok_or_else(expired)?;
    let client_id = creds.client_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(expired)?;
    let client_secret = creds.client_secret.as_deref().filter(|s| !s.is_empty()).ok_or_else(expired)?;
    let req = ctx
        .http
        .client
        .post(format!("{}/oauth/v2/accessToken", ctx.base("linkedin.auth", AUTH)))
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ]);
    match http::send_json::<Value>(req, "LinkedIn token refresh").await {
        Ok(token) => token_credentials(&token, client_id, client_secret, Some(refresh_token)),
        // Refresh token expired (they last a year) or revoked.
        Err(ProviderError::Invalid(m) | ProviderError::Unauthorized(m) | ProviderError::Forbidden(m)) => Err(
            ProviderError::Unauthorized(format!("LinkedIn did not renew the sign-in ({m}). Reconnect the account.")),
        ),
        Err(e) => Err(e),
    }
}

async fn userinfo(ctx: &Ctx, creds: &Credentials) -> PResult<Value> {
    let resp = send(ctx.http.client.get(format!("{}/v2/userinfo", api(ctx))).bearer_auth(creds.token())).await?;
    http::read_json(resp, "LinkedIn profile").await
}

fn member_name(me: &Value) -> String {
    non_empty(js(me, "/name"))
        .or_else(|| {
            let full = format!("{} {}", js(me, "/given_name").unwrap_or_default(), js(me, "/family_name").unwrap_or_default());
            non_empty(Some(full.trim().to_string()))
        })
        .unwrap_or_else(|| "LinkedIn member".into())
}

pub async fn profile(ctx: &Ctx, _account: &Account, creds: &Credentials) -> PResult<ProfileUpdate> {
    let me = userinfo(ctx, creds).await?;
    let name = member_name(&me);
    Ok(ProfileUpdate {
        handle: Some(name.clone()),
        display_name: Some(name),
        avatar_url: non_empty(js(&me, "/picture")),
        profile_url: None,
        // Follower counts need partner-only permissions.
        stats: ProfileStats::default(),
        meta: serde_json::Map::new(),
    })
}

// ---------------------------------------------------------------- posting

/// Text for a post's `commentary` in LinkedIn's `little` format: every
/// reserved character gets a backslash, except a `#` that starts a hashtag.
pub(crate) fn little_text(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len() + 8);
    for (i, &c) in chars.iter().enumerate() {
        if RESERVED.contains(&c) && !(c == '#' && starts_hashtag(&chars, i)) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `#word` at the start, after white space or an opening bracket, where the
/// word (letters and digits) is not just a number.
fn starts_hashtag(chars: &[char], i: usize) -> bool {
    let at_boundary = i == 0 || chars[i - 1].is_whitespace() || matches!(chars[i - 1], '(' | '[' | '{' | '"' | '\'' | '“' | '‘');
    let word: Vec<&char> = chars[i + 1..].iter().take_while(|c| c.is_alphanumeric()).collect();
    at_boundary && word.iter().any(|c| !c.is_numeric())
}

fn visibility(v: Option<&str>) -> &'static str {
    match v {
        // "followers"/"private" come from the shared composer options of other
        // networks: keep such posts away from the public.
        Some("connections" | "followers" | "private" | "direct") => "CONNECTIONS",
        _ => "PUBLIC",
    }
}

fn person_urn(account: &Account) -> PResult<String> {
    let id = account.remote_id.trim();
    if id.is_empty() || id.contains(|c: char| c == ':' || c == '/' || c.is_whitespace()) {
        return Err(ProviderError::invalid("This LinkedIn account has no member id. Reconnect it."));
    }
    Ok(format!("urn:li:person:{id}"))
}

fn post_url(urn: &str) -> String {
    format!("{WEB}/feed/update/{urn}/")
}

/// The image upload needs the member's token; only send it to LinkedIn
/// (or to the API host the app was pointed at, in tests).
fn may_send_token(upload_url: &str, api_base: &str) -> bool {
    let (Ok(u), Ok(base)) = (url::Url::parse(upload_url), url::Url::parse(api_base)) else {
        return false;
    };
    let host = u.host_str().unwrap_or_default().to_ascii_lowercase();
    let linkedin = u.scheme() == "https" && (host == "linkedin.com" || host.ends_with(".linkedin.com"));
    let same_as_api = u.scheme() == base.scheme()
        && u.host_str() == base.host_str()
        && u.port_or_known_default() == base.port_or_known_default();
    linkedin || same_as_api
}

/// Registers an image with the Images API, uploads it and returns its URN.
async fn upload_image(ctx: &Ctx, creds: &Credentials, owner: &str, img: PreparedImage, version: &mut String) -> PResult<String> {
    let base = api(ctx);
    let url = format!("{base}/rest/images?action=initializeUpload");
    let body = json!({ "initializeUploadRequest": { "owner": owner } });
    let resp = versioned(creds, version, || ctx.http.client.post(&url).json(&body)).await?;
    let v: Value = http::read_json(resp, "LinkedIn image upload").await?;
    let upload_url = js(&v, "/value/uploadUrl").ok_or_else(|| ProviderError::parse("LinkedIn image upload", "no uploadUrl"))?;
    let urn = js(&v, "/value/image").ok_or_else(|| ProviderError::parse("LinkedIn image upload", "no image URN"))?;
    let mut req = ctx.http.client.put(&upload_url).body(img.bytes);
    if may_send_token(&upload_url, &base) {
        req = req.bearer_auth(creds.token());
    }
    send(req).await?;
    Ok(urn)
}

fn image_ref(urn: String, alt: &str) -> Value {
    let mut v = json!({ "id": urn });
    let alt = alt.trim();
    if !alt.is_empty() {
        v["altText"] = json!(alt.chars().take(ALT_TEXT_MAX).collect::<String>());
    }
    v
}

pub fn validate(_account: &Account, post: &OutgoingPost) -> Vec<String> {
    let mut problems = Vec::new();
    if post.reply_to.is_some() {
        problems.push("LinkedIn posts from this app can't be replies.".to_string());
    }
    problems
}

pub async fn publish(ctx: &Ctx, account: &Account, creds: &Credentials, post: &OutgoingPost) -> PResult<PublishResult> {
    if let Some(problem) = validate(account, post).into_iter().next() {
        return Err(ProviderError::Invalid(problem));
    }
    let author = person_urn(account)?;
    let mut version = current_version();
    let mut images = Vec::new();
    for (img, alt) in super::prepare_images(account, post)? {
        let urn = upload_image(ctx, creds, &author, img, &mut version).await?;
        images.push(image_ref(urn, &alt));
    }
    let mut body = json!({
        "author": author,
        "commentary": little_text(&post.text),
        "visibility": visibility(post.options.visibility.as_deref()),
        "distribution": {
            "feedDistribution": "MAIN_FEED",
            "targetEntities": [],
            "thirdPartyDistributionChannels": []
        },
        "lifecycleState": "PUBLISHED",
        "isReshareDisabledByAuthor": false
    });
    match images.len() {
        0 => {}
        1 => body["content"] = json!({ "media": images.remove(0) }),
        _ => body["content"] = json!({ "multiImage": { "images": images } }),
    }
    let url = format!("{}/rest/posts", api(ctx));
    let resp = versioned(creds, &mut version, || ctx.http.client.post(&url).json(&body)).await?;
    // The new post's URN comes back in the x-restli-id header.
    let header_urn = resp.headers().get("x-restli-id").and_then(|v| v.to_str().ok()).map(str::to_string);
    let urn = match header_urn {
        Some(urn) => urn,
        None => {
            let bytes = resp.bytes().await.unwrap_or_default();
            serde_json::from_slice::<Value>(&bytes).ok().and_then(|v| js(&v, "/id")).unwrap_or_default()
        }
    };
    if urn.is_empty() {
        log::warn!("LinkedIn created the post but did not say its id");
        return Ok(PublishResult { remote_id: String::new(), url: None });
    }
    Ok(PublishResult { url: Some(post_url(&urn)), remote_id: urn })
}

pub async fn stream(
    _ctx: &Ctx,
    _account: &Account,
    _creds: &Credentials,
    _kind: &str,
    _param: Option<&str>,
    _cursor: Option<&str>,
) -> PResult<FeedPage> {
    Err(ProviderError::unsupported("LinkedIn does not let apps like this one read feeds."))
}

pub async fn interact(_ctx: &Ctx, _account: &Account, _creds: &Credentials, _target: &Value, _action: Interaction) -> PResult<Value> {
    Err(ProviderError::unsupported("LinkedIn does not let apps like this one react to posts."))
}

pub async fn delete(ctx: &Ctx, _account: &Account, creds: &Credentials, remote_id: &str) -> PResult<()> {
    let urn = remote_id.trim();
    if !urn.starts_with("urn:li:") {
        return Err(ProviderError::invalid("This is not a LinkedIn post id."));
    }
    let encoded: String = url::form_urlencoded::byte_serialize(urn.as_bytes()).collect();
    let url = format!("{}/rest/posts/{encoded}", api(ctx));
    let mut version = current_version();
    versioned(creds, &mut version, || ctx.http.client.delete(&url).header("X-RestLi-Method", "DELETE"))
        .await
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oauth::Pkce;
    use crate::testutil::bytes_contains;
    use chrono::NaiveDate;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{body_partial_json, body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    fn ctx(server: &MockServer) -> Ctx {
        Ctx::default()
            .with_override("linkedin.api", &server.uri())
            .with_override("linkedin.auth", &server.uri())
    }

    fn account() -> Account {
        Account {
            id: "a1".into(),
            platform: Platform::Linkedin,
            server: String::new(),
            remote_id: "782bbtaQ".into(),
            handle: "John Doe".into(),
            display_name: "John Doe".into(),
            avatar_url: None,
            profile_url: None,
            meta: json!({}),
            status: AccountStatus::Ok,
            status_message: None,
            position: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn oauth_begin(secret: Option<&str>) -> OAuthBegin {
        OAuthBegin {
            platform: Platform::Linkedin,
            fields: HashMap::new(),
            redirect_uri: "http://localhost:5555/callback".into(),
            state: "st".into(),
            pkce: Pkce::new(),
            client_id: Some("861hhm46p48to2".into()),
            client_secret: secret.map(str::to_string),
        }
    }

    fn userinfo_body() -> Value {
        json!({
            "sub": "782bbtaQ",
            "name": "John Doe",
            "given_name": "John",
            "family_name": "Doe",
            "picture": "https://media.licdn-ei.com/dms/image/C5F03AQHqK8v7tB1HCQ/profile-displayphoto-shrink_100_100/0/",
            "locale": "en-US",
            "email": "doe@email.com",
            "email_verified": true
        })
    }

    fn image_file(dir: &std::path::Path, name: &str, alt: &str) -> MediaAttachment {
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 6, image::Rgb([10, 20, 30])))
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        let mut att = crate::media::import_bytes(dir, &bytes, name).unwrap();
        att.alt = alt.into();
        att
    }

    fn outgoing(text: &str, media: Vec<MediaAttachment>, visibility: Option<&str>) -> OutgoingPost {
        OutgoingPost {
            text: text.into(),
            media,
            options: PostOptions { visibility: visibility.map(str::to_string), ..Default::default() },
            reply_to: None,
            idempotency_key: "k".into(),
        }
    }

    #[test]
    fn computes_api_versions() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(api_version_for(d(2026, 11, 15)), "202608");
        assert_eq!(api_version_for(d(2026, 3, 1)), "202512");
        assert_eq!(api_version_for(d(2026, 1, 31)), "202510");
        assert_eq!(api_version_for(d(2027, 12, 31)), "202709");
        // December is usually skipped, so the next month is tried first.
        assert_eq!(version_candidates("202512"), vec!["202512", "202601", "202511", "202602", "202510"]);
        assert_eq!(version_candidates("202601"), vec!["202601", "202602", "202512", "202603", "202511"]);
        assert_eq!(version_candidates("bogus"), vec!["bogus"]);
        assert_eq!(month_index("202613"), None);
        assert!(version_not_active(StatusCode::UPGRADE_REQUIRED, ""));
        assert!(version_not_active(
            StatusCode::BAD_REQUEST,
            r#"{"status": 426, "code": "NONEXISTENT_VERSION", "message": "Requested version 20260800 is not active"}"#
        ));
        assert!(!version_not_active(StatusCode::BAD_REQUEST, r#"{"status": 400, "code": "MISSING_FIELD"}"#));
    }

    #[test]
    fn escapes_little_text() {
        assert_eq!(little_text("Hello (world) #rust @me"), r"Hello \(world\) #rust \@me");
        assert_eq!(little_text("#first, C# and #1 fan ##x"), r"#first, C\# and \#1 fan \#\#x");
        assert_eq!(
            little_text(r"a_b *bold* ~x~ <tag> {x} [y] | back\slash"),
            r"a\_b \*bold\* \~x\~ \<tag\> \{x\} \[y\] \| back\\slash"
        );
        assert_eq!(little_text("(#rust) #día #日本\n#next"), "\\(#rust\\) #día #日本\n#next");
        assert_eq!(little_text("https://example.com/a_b#frag"), r"https://example.com/a\_b\#frag");
        assert_eq!(little_text("＃tag and plain text."), "＃tag and plain text.");
    }

    #[tokio::test]
    async fn signs_in_with_client_secret_and_openid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/v2/accessToken"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=AQTQmah11lalyH65DAIivsjsAQV5P"))
            .and(body_string_contains("client_id=861hhm46p48to2"))
            .and(body_string_contains("client_secret=gPecS7yqHkyyShvR"))
            .and(body_string_contains("redirect_uri=http%3A%2F%2Flocalhost%3A5555%2Fcallback"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "AQUvlL_DYEzvT2wz1QJiEPeLioeA",
                "expires_in": 5184000,
                "scope": "openid,profile,w_member_social"
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v2/userinfo"))
            .and(header("authorization", "Bearer AQUvlL_DYEzvT2wz1QJiEPeLioeA"))
            .respond_with(ResponseTemplate::new(200).set_body_json(userinfo_body()))
            .mount(&server)
            .await;
        let ctx = ctx(&server);
        assert!(matches!(begin(&ctx, &oauth_begin(None)).await, Err(ProviderError::Invalid(_))));
        let b = oauth_begin(Some("gPecS7yqHkyyShvR"));
        let start = begin(&ctx, &b).await.unwrap();
        assert!(start.auth_url.starts_with(&format!("{}/oauth/v2/authorization?", server.uri())));
        for part in [
            "response_type=code",
            "client_id=861hhm46p48to2",
            "redirect_uri=http%3A%2F%2Flocalhost%3A5555%2Fcallback",
            "state=st",
            "scope=openid%20profile%20w_member_social",
        ] {
            assert!(start.auth_url.contains(part), "{part} missing in {}", start.auth_url);
        }
        assert!(!start.auth_url.contains("code_challenge"));
        assert!(!start.auth_url.contains("gPecS7yqHkyyShvR"), "the secret must not be in the URL");

        let cancelled = HashMap::from([
            ("error".to_string(), "user_cancelled_authorize".to_string()),
            ("error_description".to_string(), "The user cancelled the authorization".to_string()),
            ("state".to_string(), "st".to_string()),
        ]);
        assert_eq!(
            finish(&ctx, &b, &start.flow, &cancelled).await.unwrap_err(),
            ProviderError::Forbidden("Access was declined in the browser.".into())
        );
        let code = "AQTQmah11lalyH65DAIivsjsAQV5P-1VTVVebnLl_SCiyMXoIjDmJ4s6rO1VBGP5Hx2542KaR_eNawkrWiCiAGxIaV";
        let wrong_state = HashMap::from([("code".to_string(), code.to_string()), ("state".to_string(), "x".to_string())]);
        assert!(finish(&ctx, &b, &start.flow, &wrong_state).await.is_err());

        let params = HashMap::from([("code".to_string(), code.to_string()), ("state".to_string(), "st".to_string())]);
        let accounts = finish(&ctx, &b, &start.flow, &params).await.unwrap();
        let a = &accounts[0];
        assert_eq!(a.platform, Platform::Linkedin);
        assert_eq!(a.server, "");
        assert_eq!(a.remote_id, "782bbtaQ");
        assert_eq!(a.handle, "John Doe");
        assert_eq!(a.display_name, "John Doe");
        assert!(a.avatar_url.as_deref().unwrap().starts_with("https://media.licdn-ei.com/"));
        let c = &a.credentials;
        assert_eq!(c.access_token.as_deref(), Some("AQUvlL_DYEzvT2wz1QJiEPeLioeA"));
        assert_eq!(c.refresh_token, None);
        assert_eq!(c.client_id.as_deref(), Some("861hhm46p48to2"));
        assert_eq!(c.client_secret.as_deref(), Some("gPecS7yqHkyyShvR"));
        let exp = c.expires_at.unwrap() - now_secs();
        assert!((5_183_990..=5_184_000).contains(&exp), "{exp}");

        let p = profile(&ctx, &account(), &Credentials::bearer("AQUvlL_DYEzvT2wz1QJiEPeLioeA")).await.unwrap();
        assert_eq!(p.display_name.as_deref(), Some("John Doe"));
        assert_eq!(p.stats, ProfileStats::default());
    }

    #[tokio::test]
    async fn refreshes_only_with_a_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/v2/accessToken"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=AQWAft_WjYZKwuWXLC5hQlghgTam"))
            .and(body_string_contains("client_secret=gPecS7yqHkyyShvR"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "BBBB2kXITHELmWblJigbHEuoFdfRhOwGA0QNnumBI8XOVSs0HtOHEU",
                "expires_in": 86400,
                "refresh_token": "AQWAft_WjYZKwuWXLC5hQlghgTam-new",
                "refresh_token_expires_in": 439200,
                "scope": "openid,profile,w_member_social"
            })))
            .expect(1)
            .mount(&server)
            .await;
        let mut creds = Credentials::bearer("old");
        creds.client_id = Some("861hhm46p48to2".into());
        creds.client_secret = Some("gPecS7yqHkyyShvR".into());
        assert_eq!(
            refresh(&ctx(&server), &account(), &creds).await.unwrap_err(),
            ProviderError::Unauthorized(EXPIRED.into())
        );
        creds.refresh_token = Some("AQWAft_WjYZKwuWXLC5hQlghgTam".into());
        let new = refresh(&ctx(&server), &account(), &creds).await.unwrap();
        assert_eq!(new.access_token.as_deref(), Some("BBBB2kXITHELmWblJigbHEuoFdfRhOwGA0QNnumBI8XOVSs0HtOHEU"));
        assert_eq!(new.refresh_token.as_deref(), Some("AQWAft_WjYZKwuWXLC5hQlghgTam-new"));
        assert!(new.extra.get("refreshTokenExpiresAt").and_then(Value::as_i64).unwrap() > now_secs() + 439_000);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/v2/accessToken"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_request",
                "error_description": "The provided authorization grant or refresh token is invalid, expired or revoked"
            })))
            .mount(&server)
            .await;
        match refresh(&ctx(&server), &account(), &creds).await {
            Err(ProviderError::Unauthorized(m)) => assert!(m.contains("invalid, expired or revoked"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn publishes_text_in_little_format() {
        let server = MockServer::start().await;
        let version = current_version();
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .and(header("authorization", "Bearer tok"))
            .and(header("linkedin-version", version.as_str()))
            .and(header("x-restli-protocol-version", "2.0.0"))
            .and(body_partial_json(json!({
                "author": "urn:li:person:782bbtaQ",
                "commentary": "Hello \\(world\\) #rust \\@me",
                "visibility": "CONNECTIONS",
                "distribution": {"feedDistribution": "MAIN_FEED", "targetEntities": [], "thirdPartyDistributionChannels": []},
                "lifecycleState": "PUBLISHED",
                "isReshareDisabledByAuthor": false
            })))
            .respond_with(ResponseTemplate::new(201).insert_header("x-restli-id", "urn:li:share:6844785523593134080"))
            .expect(1)
            .mount(&server)
            .await;
        let post = outgoing("Hello (world) #rust @me", vec![], Some("connections"));
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &post).await.unwrap();
        assert_eq!(res.remote_id, "urn:li:share:6844785523593134080");
        assert_eq!(res.url.as_deref(), Some("https://www.linkedin.com/feed/update/urn:li:share:6844785523593134080/"));
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert!(body.get("content").is_none(), "{body}");

        let mut reply = outgoing("hi", vec![], None);
        reply.reply_to = Some(json!({"id": "x"}));
        assert!(matches!(publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &reply).await, Err(ProviderError::Invalid(_))));
        assert_eq!(visibility(Some("public")), "PUBLIC");
        assert_eq!(visibility(None), "PUBLIC");
        assert_eq!(visibility(Some("followers")), "CONNECTIONS");
    }

    async fn mount_image_uploads(server: &MockServer) {
        let n = Arc::new(AtomicUsize::new(0));
        let base = server.uri();
        Mock::given(method("POST"))
            .and(path("/rest/images"))
            .and(query_param("action", "initializeUpload"))
            .and(header("linkedin-version", current_version().as_str()))
            .and(body_partial_json(json!({"initializeUploadRequest": {"owner": "urn:li:person:782bbtaQ"}})))
            .respond_with(move |_: &Request| {
                let i = n.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "value": {
                        "uploadUrlExpiresAt": 1650567510704u64,
                        "uploadUrl": format!("{base}/dms-uploads/C4E10AQFoyyAjHPMQu{i}/uploaded-image/0?ca=vector_ads&cn=uploads&sync=0&v=beta&ut=08zHQjMjAOLqc1"),
                        "image": format!("urn:li:image:C4E10AQFoyyAjHPMQu{i}")
                    }
                }))
            })
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(header("authorization", "Bearer tok"))
            .and(query_param("ut", "08zHQjMjAOLqc1"))
            .and(bytes_contains("IHDR"))
            .respond_with(ResponseTemplate::new(201))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn publishes_single_and_multi_image_posts() {
        let server = MockServer::start().await;
        mount_image_uploads(&server).await;
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .and(body_partial_json(json!({"content": {"media": {"id": "urn:li:image:C4E10AQFoyyAjHPMQu0", "altText": "A red square"}}})))
            .respond_with(ResponseTemplate::new(201).insert_header("x-restli-id", "urn:li:share:1"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .and(body_partial_json(json!({"visibility": "PUBLIC", "content": {"multiImage": {"images": [
                {"id": "urn:li:image:C4E10AQFoyyAjHPMQu1", "altText": "First"},
                {"id": "urn:li:image:C4E10AQFoyyAjHPMQu2"}
            ]}}})))
            .respond_with(ResponseTemplate::new(201).insert_header("x-restli-id", "urn:li:ugcPost:68447855235931240"))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let (ctx, acc, creds) = (ctx(&server), account(), Credentials::bearer("tok"));
        let one = outgoing("", vec![image_file(dir.path(), "a.png", "A red square")], None);
        assert_eq!(publish(&ctx, &acc, &creds, &one).await.unwrap().remote_id, "urn:li:share:1");
        let two = outgoing(
            "Two pictures",
            vec![image_file(dir.path(), "b.png", "First"), image_file(dir.path(), "c.png", "")],
            Some("public"),
        );
        let res = publish(&ctx, &acc, &creds, &two).await.unwrap();
        assert_eq!(res.url.as_deref(), Some("https://www.linkedin.com/feed/update/urn:li:ugcPost:68447855235931240/"));
        let puts = server.received_requests().await.unwrap().into_iter().filter(|r| r.method.as_str() == "PUT").count();
        assert_eq!(puts, 3);
    }

    #[tokio::test]
    async fn retries_a_neighbouring_version_when_not_active() {
        let server = MockServer::start().await;
        let first = current_version();
        let next = version_candidates(&first)[1].clone();
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .and(header("linkedin-version", first.as_str()))
            .respond_with(ResponseTemplate::new(426).set_body_json(json!({
                "status": 426, "code": "NONEXISTENT_VERSION", "message": format!("Requested version {first}00 is not active")
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .and(header("linkedin-version", next.as_str()))
            .respond_with(ResponseTemplate::new(201).insert_header("x-restli-id", "urn:li:share:2"))
            .expect(1)
            .mount(&server)
            .await;
        let res = publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &outgoing("hi", vec![], None))
            .await
            .unwrap();
        assert_eq!(res.remote_id, "urn:li:share:2");

        // When no month works the error says so instead of looping.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .respond_with(ResponseTemplate::new(426).set_body_json(json!({
                "status": 426, "code": "NONEXISTENT_VERSION", "message": "Requested version 20260800 is not active"
            })))
            .expect(5)
            .mount(&server)
            .await;
        match publish(&ctx(&server), &account(), &Credentials::bearer("tok"), &outgoing("hi", vec![], None)).await {
            Err(ProviderError::Other(m)) => assert!(m.contains("computer's date"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn deletes_post_by_encoded_urn() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/posts/urn%3Ali%3Ashare%3A6844785523593134080"))
            .and(header("x-restli-method", "DELETE"))
            .and(header("x-restli-protocol-version", "2.0.0"))
            .and(header("linkedin-version", current_version().as_str()))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let (ctx, acc, creds) = (ctx(&server), account(), Credentials::bearer("tok"));
        delete(&ctx, &acc, &creds, "urn:li:share:6844785523593134080").await.unwrap();
        assert!(delete(&ctx, &acc, &creds, "../me").await.is_err());
    }

    #[tokio::test]
    async fn maps_errors_and_unsupported_actions() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/userinfo"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "serviceErrorCode": 65600, "message": "Invalid access token", "status": 401
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/rest/posts"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "status": 403, "serviceErrorCode": 100, "code": "ACCESS_DENIED",
                "message": "Not enough permissions to access: partnerApiPostsExternal.CREATE.20260801"
            })))
            .mount(&server)
            .await;
        let (ctx, acc, creds) = (ctx(&server), account(), Credentials::bearer("tok"));
        match profile(&ctx, &acc, &creds).await {
            Err(ProviderError::Unauthorized(m)) => assert!(m.contains("Invalid access token") && m.contains("60 days"), "{m}"),
            other => panic!("{other:?}"),
        }
        match publish(&ctx, &acc, &creds, &outgoing("hi", vec![], None)).await {
            Err(ProviderError::Forbidden(m)) => assert!(m.contains("Share on LinkedIn"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(stream(&ctx, &acc, &creds, "home", None, None).await, Err(ProviderError::Unsupported(_))));
        assert!(matches!(
            interact(&ctx, &acc, &creds, &json!({}), Interaction::Like).await,
            Err(ProviderError::Unsupported(_))
        ));
        assert!(may_send_token("https://www.linkedin.com/dms-uploads/x/uploaded-image/0", API));
        assert!(!may_send_token("http://www.linkedin.com/dms-uploads/x", API));
        assert!(!may_send_token("https://evil.example/linkedin.com", API));
    }
}
