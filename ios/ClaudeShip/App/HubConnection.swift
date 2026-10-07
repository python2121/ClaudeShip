import Foundation

/// How to reach one paired hub and prove we're paired with it. Requests
/// carry the token as the same cookie a paired browser would send, so the
/// hub treats the phone exactly like the web app. Persistence (the list of
/// hubs, the Keychain items) is `HubRegistry`'s business.
@Observable
final class HubConnection {
    static let cookieName = "claude_ship"

    let baseURL: URL
    /// Replaced in place when the same hub is paired again (a rotated secret).
    var token: String?

    /// No cookie jar: the token is attached by hand, and a Set-Cookie from
    /// the hub must not end up stored twice.
    let session: URLSession = {
        let config = URLSessionConfiguration.ephemeral
        config.httpShouldSetCookies = false
        config.httpCookieAcceptPolicy = .never
        config.timeoutIntervalForRequest = 8
        config.waitsForConnectivity = false
        return URLSession(configuration: config)
    }()

    init(baseURL: URL, token: String?) {
        self.baseURL = baseURL
        self.token = token
    }

    var isPaired: Bool { token != nil }

    /// The host as a person would say it ("100.101.102.103:7433").
    var displayAddress: String { Self.displayAddress(baseURL) }

    static func displayAddress(_ url: URL) -> String {
        guard let host = url.host else { return url.absoluteString }
        return url.port.map { "\(host):\($0)" } ?? host
    }

    /// The pairing link from `claudeship hub link`.
    /// Returns the hub's base URL and the token.
    static func parse(link: String) -> (URL, String)? {
        let text = link.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let components = URLComponents(string: text), let host = components.host, !host.isEmpty,
              let token = components.queryItems?.first(where: { $0.name == "k" })?.value,
              isToken(token)
        else { return nil }
        var base = URLComponents()
        base.scheme = components.scheme ?? "http"
        base.host = host
        base.port = components.port ?? 7433
        guard let url = base.url else { return nil }
        return (url, token)
    }

    static func isToken(_ text: String) -> Bool {
        text.count == 64 && text.allSatisfy { $0.isHexDigit && ($0.isNumber || $0.isLowercase) }
    }

    func request(_ path: String, method: String = "GET", json: [String: Any]? = nil) -> URLRequest? {
        guard let url = URL(string: path, relativeTo: baseURL) else { return nil }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.cachePolicy = .reloadIgnoringLocalCacheData
        if let token { request.setValue("\(Self.cookieName)=\(token)", forHTTPHeaderField: "Cookie") }
        if let json {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try? JSONSerialization.data(withJSONObject: json)
        }
        return request
    }

    /// The terminal WebSocket for a session. `claim` says whether opening
    /// it should size the session for this screen.
    func terminalRequest(hubId: String, cols: Int, rows: Int, claim: Bool) -> URLRequest? {
        guard var components = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else { return nil }
        components.scheme = baseURL.scheme == "https" ? "wss" : "ws"
        components.path = "/ws/term"
        components.queryItems = [
            URLQueryItem(name: "id", value: hubId),
            URLQueryItem(name: "rows", value: String(rows)),
            URLQueryItem(name: "cols", value: String(cols)),
            URLQueryItem(name: "claim", value: claim ? "1" : "0"),
        ]
        guard let url = components.url else { return nil }
        var request = URLRequest(url: url)
        if let token { request.setValue("\(Self.cookieName)=\(token)", forHTTPHeaderField: "Cookie") }
        return request
    }
}
