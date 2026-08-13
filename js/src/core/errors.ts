/**
 * A single error type for every way the admin-auth contract can refuse a
 * request. `status` is the HTTP status the caller (Nest guard, middleware)
 * should respond with — see contract/README.md rule 5 for why 401 vs 503
 * must never collapse into each other.
 */
export type AdminGuardErrorStatus = 401 | 403 | 503;

export class AdminGuardError extends Error {
  readonly status: AdminGuardErrorStatus;

  constructor(status: AdminGuardErrorStatus, message: string) {
    super(message);
    this.name = 'AdminGuardError';
    this.status = status;
  }
}
