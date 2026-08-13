import {
  ForbiddenException,
  ServiceUnavailableException,
  UnauthorizedException,
} from '@nestjs/common';

import { AdminGuardError } from '../core/errors';

/** Maps the core's status-carrying error onto the matching Nest HTTP exception. */
export function toNestException(err: unknown): Error {
  if (err instanceof AdminGuardError) {
    switch (err.status) {
      case 401:
        return new UnauthorizedException(err.message);
      case 403:
        return new ForbiddenException(err.message);
      case 503:
        return new ServiceUnavailableException(err.message);
    }
  }
  return err instanceof Error ? err : new Error(String(err));
}
