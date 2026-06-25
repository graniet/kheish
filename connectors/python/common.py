import base64
import hmac
import http.client
import ipaddress
import json
import logging
import mimetypes
import os
import signal
import socket
import ssl
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, Iterable, Optional, Tuple
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urljoin, urlsplit
from urllib.request import Request, urlopen


LOG = logging.getLogger("kheish_external_connectors")
logging.basicConfig(
    level=os.environ.get("KHEISH_EXTERNAL_CONNECTOR_LOG_LEVEL", "INFO").upper(),
    format="[%(levelname)s] %(message)s",
)

RUNTIME_PROTOCOL_VERSION = 1
INGRESS_PROTOCOL_VERSION = 2
LEGACY_INGRESS_PROTOCOL_VERSION = 1
DEFAULT_MAX_REQUEST_BYTES = 2 * 1024 * 1024
DEFAULT_MAX_FETCH_BYTES = 64 * 1024 * 1024
DEFAULT_FETCH_TIMEOUT_SECONDS = 15.0
PRIVATE_NETWORKS = (
    ipaddress.ip_network("127.0.0.0/8"),
    ipaddress.ip_network("10.0.0.0/8"),
    ipaddress.ip_network("172.16.0.0/12"),
    ipaddress.ip_network("192.168.0.0/16"),
    ipaddress.ip_network("169.254.0.0/16"),
    ipaddress.ip_network("::1/128"),
    ipaddress.ip_network("fc00::/7"),
    ipaddress.ip_network("fe80::/10"),
)


class RetryableDeliveryError(RuntimeError):
    pass


class TerminalDeliveryError(RuntimeError):
    pass


def bool_env(name: str, default: bool = False) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def json_env(name: str, default: Any) -> Any:
    raw = os.environ.get(name, "").strip()
    if not raw:
        return default
    try:
        return json.loads(raw)
    except json.JSONDecodeError as exc:
        raise RuntimeError(f"invalid JSON in {name}: {exc}") from exc


def parse_base_url(base_url: str) -> Tuple[str, int]:
    parsed = urlsplit(base_url)
    if parsed.scheme != "http":
        raise RuntimeError(f"external sidecar base_url must use http://, got {base_url}")
    if not parsed.hostname or not parsed.port:
        raise RuntimeError(f"external sidecar base_url must include host and port, got {base_url}")
    return parsed.hostname, parsed.port


def is_loopback_host(host: str) -> bool:
    return host in {"127.0.0.1", "localhost", "::1"}


def route_encode(payload: Dict[str, Any]) -> str:
    return json.dumps(payload, separators=(",", ":"), sort_keys=True)


def route_decode(payload: Optional[str]) -> Dict[str, Any]:
    if not payload:
        return {}
    try:
        decoded = json.loads(payload)
    except json.JSONDecodeError:
        return {"value": payload}
    return decoded if isinstance(decoded, dict) else {"value": decoded}


def normalize_path(path: Iterable[Any]) -> list[str]:
    normalized = []
    for segment in path:
        text = str(segment).strip()
        if text:
            normalized.append(text)
    return normalized


def guess_media_type(file_name: str, declared: Optional[str] = None) -> str:
    if declared:
        return declared
    guessed, _ = mimetypes.guess_type(file_name)
    return guessed or "application/octet-stream"


def inline_asset_from_bytes(
    file_name: str,
    data: bytes,
    media_type: Optional[str] = None,
) -> Dict[str, Any]:
    return {
        "type": "inline_asset",
        "file_name": file_name,
        "media_type": guess_media_type(file_name, media_type),
        "content_base64": base64.b64encode(data).decode("ascii"),
    }


def append_jsonl(path: str, payload: Dict[str, Any]) -> None:
    target = Path(path)
    target.parent.mkdir(parents=True, exist_ok=True)
    with target.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps(payload, sort_keys=True) + "\n")


def _normalize_allowed_hosts(allowed_hosts: Optional[Iterable[str]]) -> set[str]:
    return {
        value.strip().lower()
        for value in (allowed_hosts or [])
        if str(value).strip()
    }


def _host_allowed(host: str, allowed_hosts: set[str]) -> bool:
    normalized = host.lower().rstrip(".")
    return any(
        normalized == candidate or normalized.endswith(f".{candidate}")
        for candidate in allowed_hosts
    )


def _resolve_public_connect_hosts(host: str, port: int) -> list[str]:
    try:
        infos = socket.getaddrinfo(host, port, type=socket.SOCK_STREAM)
    except socket.gaierror as exc:
        raise RuntimeError(f"failed to resolve {host}:{port}: {exc}") from exc
    connect_hosts = []
    seen = set()
    for *_rest, sockaddr in infos:
        if not sockaddr or not sockaddr[0]:
            continue
        address = ipaddress.ip_address(sockaddr[0])
        if any(address in network for network in PRIVATE_NETWORKS):
            raise RuntimeError(f"refusing private or loopback address {address} for {host}")
        if (
            address.is_multicast
            or address.is_unspecified
            or address.is_reserved
            or address.is_link_local
            or address.is_private
            or address.is_loopback
        ):
            raise RuntimeError(f"refusing non-public address {address} for {host}")
        normalized = str(address)
        if normalized not in seen:
            seen.add(normalized)
            connect_hosts.append(normalized)
    if not connect_hosts:
        raise RuntimeError(f"{host}:{port} did not resolve to any address")
    return connect_hosts


def validate_public_http_url(
    url: str,
    *,
    allowed_hosts: Optional[Iterable[str]] = None,
) -> Tuple[str, int]:
    parsed = urlsplit(url)
    if parsed.scheme not in {"http", "https"}:
        raise RuntimeError(f"unsupported URL scheme for external fetch: {url}")
    if parsed.username or parsed.password:
        raise RuntimeError(f"user info is not allowed in external fetch URL: {url}")
    if not parsed.hostname:
        raise RuntimeError(f"external fetch URL is missing a hostname: {url}")
    port = parsed.port or (443 if parsed.scheme == "https" else 80)
    normalized_allowlist = _normalize_allowed_hosts(allowed_hosts)
    if normalized_allowlist and not _host_allowed(parsed.hostname, normalized_allowlist):
        raise RuntimeError(
            f"host {parsed.hostname} is not in the configured external fetch allowlist"
        )
    _resolve_public_connect_hosts(parsed.hostname, port)
    return parsed.hostname, port


class _PinnedHTTPConnection(http.client.HTTPConnection):
    def __init__(self, connect_host: str, authority_host: str, **kwargs: Any) -> None:
        self._connect_host = connect_host
        super().__init__(authority_host, **kwargs)

    def connect(self) -> None:
        self.sock = self._create_connection(
            (self._connect_host, self.port),
            self.timeout,
            self.source_address,
        )
        if self._tunnel_host:
            self._tunnel()


class _PinnedHTTPSConnection(http.client.HTTPSConnection):
    def __init__(self, connect_host: str, authority_host: str, **kwargs: Any) -> None:
        self._connect_host = connect_host
        super().__init__(authority_host, **kwargs)

    def connect(self) -> None:
        raw_sock = self._create_connection(
            (self._connect_host, self.port),
            self.timeout,
            self.source_address,
        )
        if self._tunnel_host:
            self.sock = raw_sock
            self._tunnel()
            raw_sock = self.sock
        self.sock = self._context.wrap_socket(raw_sock, server_hostname=self.host)


def read_http_body_limited(response: Any, max_bytes: int) -> bytes:
    remaining = max_bytes + 1
    chunks = []
    while remaining > 0:
        chunk = response.read(min(64 * 1024, remaining))
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    body = b"".join(chunks)
    if len(body) > max_bytes:
        raise RuntimeError(f"response body exceeded the {max_bytes} byte fetch limit")
    return body


def http_request(
    method: str,
    url: str,
    body: Optional[bytes] = None,
    headers: Optional[Dict[str, str]] = None,
    timeout: float = 30.0,
    max_bytes: Optional[int] = None,
) -> Tuple[int, Dict[str, str], bytes]:
    request = Request(url, data=body, method=method.upper())
    for key, value in (headers or {}).items():
        request.add_header(key, value)
    try:
        with urlopen(request, timeout=timeout) as response:
            return (
                response.getcode(),
                dict(response.headers.items()),
                read_http_body_limited(response, max_bytes or DEFAULT_MAX_FETCH_BYTES),
            )
    except HTTPError as exc:
        return (
            exc.code,
            dict(exc.headers.items()),
            read_http_body_limited(exc, max_bytes or DEFAULT_MAX_FETCH_BYTES),
        )
    except URLError as exc:
        raise RuntimeError(f"{method} {url} failed: {exc}") from exc


def safe_public_http_request(
    method: str,
    url: str,
    *,
    body: Optional[bytes] = None,
    headers: Optional[Dict[str, str]] = None,
    timeout: float = DEFAULT_FETCH_TIMEOUT_SECONDS,
    max_bytes: int = DEFAULT_MAX_FETCH_BYTES,
    allowed_hosts: Optional[Iterable[str]] = None,
    max_redirects: int = 3,
) -> Tuple[int, Dict[str, str], bytes]:
    current_method = method.upper()
    current_url = url
    current_body = body
    current_headers = dict(headers or {})
    normalized_allowlist = _normalize_allowed_hosts(allowed_hosts)
    if not normalized_allowlist:
        raise TerminalDeliveryError("external fetches require an explicit allowed_hosts allowlist")
    for _ in range(max_redirects + 1):
        parsed = urlsplit(current_url)
        try:
            host, port = validate_public_http_url(current_url, allowed_hosts=normalized_allowlist)
        except RuntimeError as exc:
            raise TerminalDeliveryError(str(exc)) from exc
        connect_hosts = _resolve_public_connect_hosts(host, port)
        path = parsed.path or "/"
        if parsed.query:
            path = f"{path}?{parsed.query}"
        status = None
        response_headers: Dict[str, str] = {}
        response_body = b""
        last_error: Optional[BaseException] = None
        for connect_host in connect_hosts:
            connection: Optional[http.client.HTTPConnection] = None
            try:
                if parsed.scheme == "https":
                    connection = _PinnedHTTPSConnection(
                        connect_host,
                        host,
                        port=port,
                        timeout=timeout,
                        context=ssl.create_default_context(),
                    )
                else:
                    connection = _PinnedHTTPConnection(
                        connect_host,
                        host,
                        port=port,
                        timeout=timeout,
                    )
                connection.request(current_method, path, body=current_body, headers=current_headers)
                response = connection.getresponse()
                status = response.status
                response_headers = {key: value for key, value in response.getheaders()}
                try:
                    response_body = read_http_body_limited(response, max_bytes)
                except RuntimeError as exc:
                    raise TerminalDeliveryError(str(exc)) from exc
                finally:
                    response.close()
                break
            except (OSError, http.client.HTTPException, ssl.SSLError) as exc:
                last_error = exc
            finally:
                if connection is not None:
                    try:
                        connection.close()
                    except Exception:
                        pass
        if status is None:
            raise RetryableDeliveryError(f"{current_method} {current_url} failed: {last_error}")

        if status in {301, 302, 303, 307, 308}:
            location = response_headers.get("Location")
            if not location:
                raise TerminalDeliveryError(
                    f"{current_method} {current_url} redirected without a Location header"
                )
            previous = urlsplit(current_url)
            current_url = urljoin(current_url, location)
            redirected = urlsplit(current_url)
            previous_origin = (
                previous.scheme,
                previous.hostname,
                previous.port or (443 if previous.scheme == "https" else 80),
            )
            redirected_origin = (
                redirected.scheme,
                redirected.hostname,
                redirected.port or (443 if redirected.scheme == "https" else 80),
            )
            if redirected_origin != previous_origin:
                raise TerminalDeliveryError(
                    f"cross-origin redirects are not allowed while fetching {url}"
                )
            if status == 303:
                current_method = "GET"
                current_body = None
                current_headers = {
                    key: value
                    for key, value in current_headers.items()
                    if key.lower() not in {"content-length", "content-type"}
                }
            continue

        return (status, response_headers, response_body)
    raise TerminalDeliveryError(f"too many redirects while fetching {url}")


def json_request(
    method: str,
    url: str,
    payload: Optional[Dict[str, Any]] = None,
    headers: Optional[Dict[str, str]] = None,
    timeout: float = 30.0,
) -> Tuple[int, Dict[str, Any]]:
    merged_headers = {"Content-Type": "application/json"}
    merged_headers.update(headers or {})
    body = None if payload is None else json.dumps(payload).encode("utf-8")
    status, _response_headers, response_body = http_request(
        method,
        url,
        body=body,
        headers=merged_headers,
        timeout=timeout,
    )
    if not response_body:
        return status, {}
    decoded = json.loads(response_body.decode("utf-8"))
    return status, decoded


def iter_delivery_assets(payload: Dict[str, Any]) -> list[Dict[str, Any]]:
    assets = []
    for part in payload.get("parts", []) or []:
        if isinstance(part, dict) and part.get("type") == "attachment":
            attachment = part.get("attachment")
            if isinstance(attachment, dict):
                assets.append(attachment)
    for artifact in payload.get("artifacts", []) or []:
        if isinstance(artifact, dict):
            assets.append(artifact)
    return assets


def write_json_response(
    handler: BaseHTTPRequestHandler,
    status: int,
    payload: Dict[str, Any],
) -> None:
    body = json.dumps(payload).encode("utf-8")
    handler.send_response(status)
    handler.send_header("Content-Type", "application/json")
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


def write_bytes_response(
    handler: BaseHTTPRequestHandler,
    status: int,
    body: bytes,
    content_type: str,
) -> None:
    handler.send_response(status)
    handler.send_header("Content-Type", content_type)
    handler.send_header("Content-Length", str(len(body)))
    handler.end_headers()
    handler.wfile.write(body)


class DaemonClient:
    def __init__(
        self,
        daemon_base_url: str,
        connector_name: str,
        shared_token: Optional[str],
        credential_token: Optional[str],
        instance_id: str,
    ) -> None:
        self.daemon_base_url = daemon_base_url.rstrip("/")
        self.connector_name = connector_name
        self.shared_token = shared_token
        self.credential_token = credential_token
        self.instance_id = instance_id

    def _auth_headers(self) -> Dict[str, str]:
        if not self.shared_token:
            return {}
        return {"Authorization": f"Bearer {self.shared_token}"}

    def _build_event_request(self, payload: Dict[str, Any]) -> Dict[str, Any]:
        return {
            "instance_id": payload.get("instance_id") or self.instance_id,
            "event_id": payload["event_id"],
            "fingerprint": payload.get("fingerprint"),
            "occurred_at_ms": payload.get("occurred_at_ms"),
            "actor_id": payload.get("actor_id"),
            "source_kind": payload.get("source_kind"),
            "intent": payload.get("intent"),
            "relation": payload.get("relation"),
            "thread": {"path": normalize_path(payload.get("thread_path") or [])},
            "routing_key": payload.get("routing_key"),
            "content": payload.get("content", ""),
            "input_items": payload.get("input_items", []),
            "attachments": payload.get("attachments", []),
            "reply_route": payload.get("reply_route"),
            "metadata": payload.get("metadata"),
        }

    @staticmethod
    def _looks_like_unsupported_protocol(decoded: Dict[str, Any]) -> bool:
        reason = decoded.get("reason")
        return isinstance(reason, str) and "unsupported protocol_version" in reason

    @staticmethod
    def _should_fallback_batch_exception(exc: Exception) -> bool:
        if isinstance(exc, RuntimeError):
            message = str(exc)
            return "HTTP 404" in message or "HTTP 405" in message
        return isinstance(exc, json.JSONDecodeError)

    def _submit_event_once(self, payload: Dict[str, Any], protocol_version: int) -> Dict[str, Any]:
        request = {
            "protocol_version": protocol_version,
            **self._build_event_request(payload),
        }
        status, decoded = json_request(
            "POST",
            f"{self.daemon_base_url}/v1/connectors/external/{self.connector_name}/events",
            request,
            headers=self._auth_headers(),
            timeout=30.0,
        )
        if status >= 400:
            raise RuntimeError(f"daemon ingress returned HTTP {status}: {decoded}")
        return decoded

    def submit_event(self, payload: Dict[str, Any]) -> Dict[str, Any]:
        decoded = self._submit_event_once(payload, INGRESS_PROTOCOL_VERSION)
        if decoded.get("status") == "rejected" and self._looks_like_unsupported_protocol(decoded):
            return self._submit_event_once(payload, LEGACY_INGRESS_PROTOCOL_VERSION)
        return decoded

    def _submit_events_batch_once(
        self,
        payloads: Iterable[Dict[str, Any]],
        protocol_version: int,
    ) -> Dict[str, Any]:
        request = {
            "protocol_version": protocol_version,
            "events": [self._build_event_request(payload) for payload in payloads],
        }
        status, decoded = json_request(
            "POST",
            f"{self.daemon_base_url}/v1/connectors/external/{self.connector_name}/events/batch",
            request,
            headers=self._auth_headers(),
            timeout=30.0,
        )
        if status >= 400:
            raise RuntimeError(f"daemon ingress batch returned HTTP {status}: {decoded}")
        return decoded

    def submit_events_batch(self, payloads: Iterable[Dict[str, Any]]) -> Dict[str, Any]:
        payload_list = list(payloads)
        try:
            decoded = self._submit_events_batch_once(payload_list, INGRESS_PROTOCOL_VERSION)
        except Exception as exc:
            if not self._should_fallback_batch_exception(exc):
                raise
            results = [
                self._submit_event_once(payload, LEGACY_INGRESS_PROTOCOL_VERSION)
                for payload in payload_list
            ]
            return {"results": results}
        if (
            isinstance(decoded.get("results"), list)
            and decoded["results"]
            and all(
                isinstance(result, dict)
                and result.get("status") == "rejected"
                and self._looks_like_unsupported_protocol(result)
                for result in decoded["results"]
            )
        ):
            results = [
                self._submit_event_once(payload, LEGACY_INGRESS_PROTOCOL_VERSION)
                for payload in payload_list
            ]
            return {"results": results}
        return decoded

    def download_asset(self, asset: Dict[str, Any]) -> bytes:
        raw_path = str(asset.get("download_path") or "")
        if not raw_path:
            raise RuntimeError("asset descriptor is missing download_path")
        parsed = urlsplit(raw_path)
        if parsed.scheme or parsed.netloc or not raw_path.startswith("/"):
            raise RuntimeError("asset download_path must be a same-daemon absolute path")
        status, _headers, body = http_request(
            "GET",
            urljoin(self.daemon_base_url + "/", raw_path.lstrip("/")),
            headers=self._auth_headers(),
            timeout=60.0,
            max_bytes=DEFAULT_MAX_FETCH_BYTES,
        )
        if status >= 400:
            raise RuntimeError(f"asset download failed with HTTP {status}")
        return body

    def download_asset_to_tempfile(self, asset: Dict[str, Any]) -> str:
        data = self.download_asset(asset)
        suffix = Path(asset.get("file_name") or "attachment.bin").suffix
        handle = tempfile.NamedTemporaryFile(delete=False, suffix=suffix)
        handle.write(data)
        handle.flush()
        handle.close()
        return handle.name

    def fetch_credential(self, env_key: str) -> str:
        if not self.credential_token:
            raise RuntimeError("credential lookup requires KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN")
        status, decoded = json_request(
            "GET",
            f"{self.daemon_base_url}/v1/connectors/external/{self.connector_name}/credentials/{quote(env_key, safe='')}",
            headers={"Authorization": f"Bearer {self.credential_token}"},
            timeout=15.0,
        )
        if status >= 400:
            raise RuntimeError(f"credential lookup for {env_key} returned HTTP {status}: {decoded}")
        value = decoded.get("value")
        if not isinstance(value, str) or not value:
            raise RuntimeError(f"credential lookup for {env_key} returned no value")
        return value


class ExternalConnectorApp:
    platform = "external"
    experimental = False
    attachments_in = True
    attachments_out = True
    threads = False

    def __init__(self) -> None:
        self.name = os.environ["KHEISH_EXTERNAL_CONNECTOR_NAME"]
        self.base_url = os.environ["KHEISH_EXTERNAL_CONNECTOR_BASE_URL"].rstrip("/")
        self.host, self.port = parse_base_url(self.base_url)
        self.daemon_base_url = (
            os.environ.get("KHEISH_EXTERNAL_CONNECTOR_DAEMON_BASE_URL", "").strip().rstrip("/")
        )
        self.shared_token = os.environ.get("KHEISH_EXTERNAL_CONNECTOR_SHARED_TOKEN")
        self.credential_token = os.environ.get("KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN")
        self.instance_id = os.environ.get(
            "KHEISH_EXTERNAL_CONNECTOR_INSTANCE_ID",
            f"{self.platform}-{uuid.uuid4().hex[:12]}",
        )
        self.capture_path = os.environ.get("KHEISH_EXTERNAL_CONNECTOR_DELIVERY_CAPTURE_PATH", "").strip()
        self.max_request_bytes = int(
            os.environ.get(
                "KHEISH_EXTERNAL_CONNECTOR_MAX_REQUEST_BYTES",
                str(DEFAULT_MAX_REQUEST_BYTES),
            )
        )
        requested_test_api = bool_env("KHEISH_EXTERNAL_CONNECTOR_ENABLE_TEST_API", False)
        self.test_mode = bool_env("KHEISH_EXTERNAL_CONNECTOR_TEST_MODE", False)
        self.enable_test_api = (
            requested_test_api
            and self.test_mode
            and bool(self.shared_token)
            and is_loopback_host(self.host)
        )
        if requested_test_api and not self.enable_test_api:
            LOG.warning(
                "ignoring test API request for %s: it requires test mode, loopback base_url, and a shared token",
                self.platform,
            )
        self._status = "starting"
        self._detail: Optional[str] = None
        self._status_lock = threading.Lock()
        self._server: Optional[ThreadingHTTPServer] = None
        self._shutdown_requested = threading.Event()
        self._inflight_deliveries = 0
        self._inflight_condition = threading.Condition()
        self._daemon = DaemonClient(
            self.daemon_base_url,
            self.name,
            self.shared_token,
            self.credential_token,
            self.instance_id,
        )
        self._load_credentials_from_daemon()

    def mark_ready(self, detail: Optional[str] = None) -> None:
        with self._status_lock:
            self._status = "ready"
            self._detail = detail

    def mark_degraded(self, detail: str) -> None:
        with self._status_lock:
            self._status = "degraded"
            self._detail = detail

    def mark_draining(self, detail: Optional[str] = None) -> None:
        with self._status_lock:
            self._status = "draining"
            self._detail = detail

    def health_payload(self) -> Dict[str, Any]:
        with self._status_lock:
            payload = {
                "protocol_version": RUNTIME_PROTOCOL_VERSION,
                "instance_id": self.instance_id,
                "status": self._status,
            }
            if self._detail:
                payload["detail"] = self._detail
            return payload

    def start(self) -> None:
        self.start_background()
        self._install_signal_handlers()
        server = ThreadingHTTPServer((self.host, self.port), self._make_handler())
        server.daemon_threads = True
        self._server = server
        LOG.info("starting %s connector on %s", self.platform, self.base_url)
        try:
            server.serve_forever()
        finally:
            self.mark_draining("server stopped")
            self._wait_for_inflight_deliveries(timeout_seconds=10.0)
            try:
                server.server_close()
            except Exception:
                pass

    def start_background(self) -> None:
        if self.enable_test_api or self.capture_path:
            self.mark_ready("test capture mode")
        else:
            self.mark_degraded("connector transport is not configured")

    def _make_handler(self):
        app = self

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                app._dispatch(self)

            def do_POST(self) -> None:
                app._dispatch(self)

            def log_message(self, _format: str, *_args: Any) -> None:
                return

        return Handler

    def _authorized(self, handler: BaseHTTPRequestHandler) -> bool:
        if not self.shared_token:
            return True
        return hmac.compare_digest(
            handler.headers.get("Authorization", ""),
            f"Bearer {self.shared_token}",
        )

    def _dispatch(self, handler: BaseHTTPRequestHandler) -> None:
        method = handler.command.upper()
        split = urlsplit(handler.path)
        path = split.path
        body = b""
        internal_path = path in {"/manifest", "/health", "/deliver"} or (
            self.enable_test_api and path == "/__test/inject"
        )
        if internal_path and self.shared_token and not self._authorized(handler):
            write_json_response(handler, 401, {"error": "unauthorized"})
            return
        if method == "POST":
            try:
                content_length = int(handler.headers.get("Content-Length", "0"))
            except ValueError:
                write_json_response(handler, 400, {"error": "invalid_content_length"})
                return
            if content_length < 0:
                write_json_response(handler, 400, {"error": "invalid_content_length"})
                return
            if content_length > self.max_request_bytes:
                write_json_response(handler, 413, {"error": "payload_too_large"})
                return
            body = handler.rfile.read(content_length)

        if self._shutdown_requested.is_set() and path not in {"/manifest", "/health"}:
            if method == "POST" and path == "/deliver":
                write_json_response(
                    handler,
                    200,
                    {"status": "retryable_error", "detail": "connector is draining"},
                )
            else:
                write_json_response(handler, 503, {"error": "draining"})
            return

        if method == "GET" and path == "/manifest":
            write_json_response(
                handler,
                200,
                {
                    "protocol_version": RUNTIME_PROTOCOL_VERSION,
                    "instance_id": self.instance_id,
                    "capabilities": self.capabilities_payload(),
                    "experimental": self.experimental,
                },
            )
            return
        if method == "GET" and path == "/health":
            write_json_response(handler, 200, self.health_payload())
            return
        if method == "POST" and path == "/deliver":
            self._handle_delivery(handler, body)
            return
        if self.enable_test_api and method == "POST" and path == "/__test/inject":
            self._handle_test_inject(handler, body)
            return

        handled = self.handle_platform_request(method, path, split.query, handler.headers, body)
        if handled is None:
            write_json_response(handler, 404, {"error": "not_found"})
            return
        status, content_type, response_body = handled
        write_bytes_response(handler, status, response_body, content_type)

    def _handle_delivery(self, handler: BaseHTTPRequestHandler, body: bytes) -> None:
        with self._inflight_condition:
            self._inflight_deliveries += 1
        try:
            payload = json.loads(body.decode("utf-8"))
            if self.capture_path:
                append_jsonl(self.capture_path, payload)
                write_json_response(handler, 200, {"status": "committed"})
                return
            self.deliver(payload)
            write_json_response(handler, 200, {"status": "committed"})
        except RetryableDeliveryError as exc:
            write_json_response(
                handler,
                200,
                {"status": "retryable_error", "detail": str(exc)},
            )
        except TerminalDeliveryError as exc:
            write_json_response(
                handler,
                200,
                {"status": "terminal_error", "detail": str(exc)},
            )
        except Exception as exc:
            LOG.exception("delivery failed")
            write_json_response(
                handler,
                200,
                {"status": "retryable_error", "detail": str(exc)},
            )
        finally:
            with self._inflight_condition:
                self._inflight_deliveries = max(self._inflight_deliveries - 1, 0)
                self._inflight_condition.notify_all()

    def _handle_test_inject(self, handler: BaseHTTPRequestHandler, body: bytes) -> None:
        try:
            payload = json.loads(body.decode("utf-8"))
            if isinstance(payload, dict) and isinstance(payload.get("events"), list):
                events = []
                for raw_event in payload["events"]:
                    if not isinstance(raw_event, dict):
                        raise RuntimeError("test batch events must be JSON objects")
                    events.append(
                        {
                            "event_id": raw_event.get("event_id") or f"test-{uuid.uuid4().hex}",
                            "fingerprint": raw_event.get("fingerprint"),
                            "occurred_at_ms": raw_event.get(
                                "occurred_at_ms",
                                int(time.time() * 1000),
                            ),
                            "actor_id": raw_event.get("actor_id", "test-user"),
                            "source_kind": raw_event.get("source_kind", self.platform),
                            "intent": raw_event.get("intent"),
                            "relation": raw_event.get("relation"),
                            "thread_path": raw_event.get("thread_path") or [],
                            "routing_key": raw_event.get("routing_key"),
                            "content": raw_event.get("content", ""),
                            "input_items": raw_event.get("input_items", []),
                            "attachments": raw_event.get("attachments", []),
                            "reply_route": raw_event.get("reply_route"),
                            "metadata": raw_event.get("metadata", {"test_inject": True}),
                        }
                    )
                response = self._daemon.submit_events_batch(events)
            else:
                event_id = payload.get("event_id") or f"test-{uuid.uuid4().hex}"
                response = self._daemon.submit_event(
                    {
                        "event_id": event_id,
                        "fingerprint": payload.get("fingerprint"),
                        "occurred_at_ms": payload.get(
                            "occurred_at_ms",
                            int(time.time() * 1000),
                        ),
                        "actor_id": payload.get("actor_id", "test-user"),
                        "source_kind": payload.get("source_kind", self.platform),
                        "intent": payload.get("intent"),
                        "relation": payload.get("relation"),
                        "thread_path": payload.get("thread_path") or [],
                        "routing_key": payload.get("routing_key"),
                        "content": payload.get("content", ""),
                        "input_items": payload.get("input_items", []),
                        "attachments": payload.get("attachments", []),
                        "reply_route": payload.get("reply_route"),
                        "metadata": payload.get("metadata", {"test_inject": True}),
                    }
                )
            write_json_response(handler, 200, response)
        except Exception as exc:
            LOG.exception("test inject failed")
            write_json_response(handler, 500, {"error": str(exc)})

    def handle_platform_request(
        self,
        method: str,
        path: str,
        query: str,
        headers: Any,
        body: bytes,
    ) -> Optional[Tuple[int, str, bytes]]:
        return None

    def deliver(self, payload: Dict[str, Any]) -> None:
        raise TerminalDeliveryError(f"{self.platform} delivery is not implemented")

    def capabilities_payload(self) -> Dict[str, Any]:
        return {
            "attachments_in": self.attachments_in,
            "attachments_out": self.attachments_out,
            "threads": self.threads,
        }

    def _load_credentials_from_daemon(self) -> None:
        raw = os.environ.get("KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON", "").strip()
        if not raw:
            return
        if not self.credential_token:
            raise RuntimeError("credential-backed sidecars require KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_TOKEN")
        try:
            env_keys = json.loads(raw)
        except json.JSONDecodeError as exc:
            raise RuntimeError(f"invalid KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON: {exc}") from exc
        if not isinstance(env_keys, list):
            raise RuntimeError("KHEISH_EXTERNAL_CONNECTOR_CREDENTIAL_KEYS_JSON must decode to a JSON list")
        for env_key in env_keys:
            key = str(env_key).strip()
            if not key:
                continue
            os.environ[key] = self._daemon.fetch_credential(key)

    def _install_signal_handlers(self) -> None:
        if threading.current_thread() is not threading.main_thread():
            return
        for sig in (signal.SIGINT, signal.SIGTERM):
            try:
                signal.signal(sig, self._signal_handler)
            except ValueError:
                return

    def _signal_handler(self, signum: int, _frame: Any) -> None:
        self._begin_shutdown(f"received signal {signum}")

    def _begin_shutdown(self, detail: str) -> None:
        if self._shutdown_requested.is_set():
            return
        self._shutdown_requested.set()
        self.mark_draining(detail)
        server = self._server
        if server is not None:
            threading.Thread(target=server.shutdown, daemon=True).start()

    def _wait_for_inflight_deliveries(self, timeout_seconds: float) -> None:
        deadline = time.time() + timeout_seconds
        with self._inflight_condition:
            while self._inflight_deliveries > 0:
                remaining = deadline - time.time()
                if remaining <= 0:
                    return
                self._inflight_condition.wait(timeout=remaining)
