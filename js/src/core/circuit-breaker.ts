/**
 * Minimal in-process circuit breaker (closed / open / half-open) guarding
 * the introspect call. Not distributed: each process instance trips
 * independently — that is intentional, this is call-level self-protection
 * for one process, not a shared health signal.
 *
 * Trip only on transport-level failure (timeout, network error, non-2xx
 * from the IdP itself) via `recordFailure`. A legitimate `{ active: false }`
 * answer from introspect is not a failure and must never be recorded here —
 * that is a normal "deny" outcome, not the IdP being unavailable.
 */
export type CircuitState = 'closed' | 'open' | 'half_open';

export class CircuitBreaker {
  private state: CircuitState = 'closed';
  private consecutiveFailures = 0;
  private openedAt = 0;

  constructor(
    private readonly failureThreshold: number,
    private readonly resetTimeoutMs: number,
  ) {}

  /**
   * Whether a call may be attempted right now. While `open`, this flips the
   * breaker to `half_open` (and permits exactly the caller that asked) once
   * `resetTimeoutMs` has elapsed since it tripped.
   */
  canAttempt(now: number = Date.now()): boolean {
    if (this.state !== 'open') {
      return true;
    }
    if (now - this.openedAt >= this.resetTimeoutMs) {
      this.state = 'half_open';
      return true;
    }
    return false;
  }

  /** A call succeeded: fully reset the breaker to `closed`. */
  recordSuccess(): void {
    this.consecutiveFailures = 0;
    this.state = 'closed';
  }

  /**
   * A call failed at the transport level. A failure while `half_open`
   * re-opens immediately (the trial call didn't recover); from `closed`,
   * the breaker opens once `failureThreshold` consecutive failures
   * accumulate.
   */
  recordFailure(now: number = Date.now()): void {
    this.consecutiveFailures += 1;
    if (
      this.state === 'half_open' ||
      this.consecutiveFailures >= this.failureThreshold
    ) {
      this.state = 'open';
      this.openedAt = now;
      return;
    }
  }

  getState(): CircuitState {
    return this.state;
  }
}
