//! Authentication providers applied to outbound requests.

use ferroly::http::Request;

/// Applies authentication to an outbound [`ferroly::http::Request`].
///
/// Providers are shared as `Arc<dyn AuthProvider>` so a client (or a GenAI
/// provider) can be configured with any scheme without changing its type.
pub trait AuthProvider: Send + Sync {
    /// Applies credentials to the request by setting the appropriate headers.
    fn apply(&self, req: &mut Request);
}

/// Bearer-token authentication (`Authorization: Bearer <token>`).
#[derive(Debug, Clone)]
pub struct BearerAuth {
    token: String,
}

impl BearerAuth {
    /// Creates a bearer-token provider.
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
        }
    }
}

impl AuthProvider for BearerAuth {
    fn apply(&self, req: &mut Request) {
        req.headers
            .set("Authorization", format!("Bearer {}", self.token));
    }
}

/// API-key authentication placed in a caller-specified header.
#[derive(Debug, Clone)]
pub struct ApiKeyAuth {
    header: String,
    key: String,
}

impl ApiKeyAuth {
    /// Creates an API-key provider that sets `header: key`.
    pub fn new(header: impl Into<String>, key: impl Into<String>) -> Self {
        Self {
            header: header.into(),
            key: key.into(),
        }
    }
}

impl AuthProvider for ApiKeyAuth {
    fn apply(&self, req: &mut Request) {
        req.headers.set(self.header.clone(), self.key.clone());
    }
}

/// HTTP Basic authentication.
#[derive(Debug, Clone)]
pub struct BasicAuth {
    user: String,
    pass: String,
}

impl BasicAuth {
    /// Creates a basic-auth provider.
    pub fn new(user: impl Into<String>, pass: impl Into<String>) -> Self {
        Self {
            user: user.into(),
            pass: pass.into(),
        }
    }
}

impl AuthProvider for BasicAuth {
    fn apply(&self, req: &mut Request) {
        let creds = base64(format!("{}:{}", self.user, self.pass).as_bytes());
        req.headers.set("Authorization", format!("Basic {creds}"));
    }
}

/// Standard base64 encoding (for HTTP Basic credentials).
fn base64(input: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((n >> 18) & 63) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            CHARS[((n >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            CHARS[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
