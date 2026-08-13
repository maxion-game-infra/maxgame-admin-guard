import { CircuitBreaker } from './circuit-breaker';
import { AdminGuardError } from './errors';
import type { AdminIntrospectionResult } from './types';
import {
  DEFAULT_BREAKER,
  DEFAULT_INTROSPECT_HEADER,
  DEFAULT_INTROSPECT_PATH,
  DEFAULT_INTROSPECT_TIMEOUT_MS,
} from './types';

/**
 * One raw introspect call. Returns the HTTP status and the parsed body (or
 * throws if the call could not be completed at all — timeout, DNS/network
 * error, or an unparseable body). Never interprets the status; that is
 * `AdminIntrospectClient`'s job. Swappable so tests can drive every
 * transport failure mode in `contract/cases.json` (`timeout`, `http` with a
 * given status, `malformed`) without a real network call or module mocking.
 */
export type IntrospectTransport = (
  token: string,
) => Promise<{ status: number; body: unknown }>;

export type AdminIntrospectClientConfig = {
  idpBaseUrl: string;
  introspectApiKey: string;
  introspectPath?: string;
  introspectHeader?: string;
  timeoutMs?: number;
  breaker?: { failureThreshold: number; resetTimeoutMs: number };
  /** Default `Date.now`. Inject for deterministic breaker tests. */
  clock?: () => number;
  /** Inject to replace the default `fetch`-based transport (tests). */
  transport?: IntrospectTransport;
};

/**
 * Thrown when the introspect call itself could not be completed — timeout,
 * network error, non-2xx from the IdP, malformed body, or the circuit
 * breaker is open. Distinct from a legitimate `{ active: false }` answer
 * (a normal return value, not a throw): callers fail closed on this with
 * 503, and must NOT treat it as "IdP is fine, this admin just isn't"
 * (contract rule 5).
 */
export class AdminIntrospectClient {
  private readonly idpBaseUrl: string;
  private readonly introspectApiKey: string;
  private readonly introspectPath: string;
  private readonly introspectHeader: string;
  private readonly timeoutMs: number;
  private readonly clock: () => number;
  private readonly breaker: CircuitBreaker;
  private readonly transport: IntrospectTransport;

  constructor(config: AdminIntrospectClientConfig) {
    this.idpBaseUrl = (config.idpBaseUrl ?? '').trim();
    this.introspectApiKey = (config.introspectApiKey ?? '').trim();

    if (!this.idpBaseUrl || !this.introspectApiKey) {
      throw new Error(
        'AdminIntrospectClient: missing idpBaseUrl or introspectApiKey',
      );
    }

    this.introspectPath = config.introspectPath ?? DEFAULT_INTROSPECT_PATH;
    this.introspectHeader =
      config.introspectHeader ?? DEFAULT_INTROSPECT_HEADER;
    this.timeoutMs = config.timeoutMs ?? DEFAULT_INTROSPECT_TIMEOUT_MS;
    this.clock = config.clock ?? Date.now;
    const breakerConfig = config.breaker ?? DEFAULT_BREAKER;
    this.breaker = new CircuitBreaker(
      breakerConfig.failureThreshold,
      breakerConfig.resetTimeoutMs,
    );
    this.transport = config.transport ?? this.defaultTransport();
  }

  private defaultTransport(): IntrospectTransport {
    const url = new URL(this.introspectPath, this.idpBaseUrl);
    return async (token: string) => {
      const controller = new AbortController();
      const timer = setTimeout(() => controller.abort(), this.timeoutMs);
      try {
        const res = await fetch(url, {
          method: 'POST',
          headers: {
            'content-type': 'application/json',
            [this.introspectHeader]: this.introspectApiKey,
          },
          body: JSON.stringify({ token }),
          signal: controller.signal,
        });
        let body: unknown;
        try {
          body = await res.json();
        } catch {
          throw new Error('Admin IdP introspect response body was not JSON');
        }
        return { status: res.status, body };
      } finally {
        clearTimeout(timer);
      }
    };
  }

  /**
   * Never throws for a token the IdP has an opinion on — only for a failure
   * to get an opinion at all (open breaker, timeout, network error, non-2xx,
   * malformed body).
   */
  async introspect(token: string): Promise<AdminIntrospectionResult> {
    if (!this.breaker.canAttempt(this.clock())) {
      throw new AdminGuardError(503, 'Admin IdP introspect circuit is open');
    }

    let result: { status: number; body: unknown };
    try {
      result = await this.transport(token);
    } catch (err) {
      this.breaker.recordFailure(this.clock());
      const message = err instanceof Error ? err.message : 'Unknown error';
      throw new AdminGuardError(
        503,
        `Admin IdP introspect call failed: ${message}`,
      );
    }

    if (result.status < 200 || result.status >= 300) {
      // Even a 401/403 FROM the IdP means our own api key is wrong, not
      // that the admin is revoked — collapsing it into 401 would hide a
      // misconfiguration behind a plausible-looking auth failure.
      this.breaker.recordFailure(this.clock());
      throw new AdminGuardError(
        503,
        `Admin IdP introspect call failed: HTTP ${result.status}`,
      );
    }

    this.breaker.recordSuccess();
    return result.body as AdminIntrospectionResult;
  }
}
