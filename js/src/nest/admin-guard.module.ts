import type { DynamicModule, FactoryProvider, Provider } from '@nestjs/common';
import { Module } from '@nestjs/common';

import { AdminIntrospectClient } from '../core/introspect-client';
import { AdminJwtVerifier } from '../core/jwt-verifier';
import type { AdminGuardConfig } from '../core/types';

export const ADMIN_GUARD_OPTIONS = 'ADMIN_GUARD_OPTIONS';

export type AdminGuardNestOptions = AdminGuardConfig & {
  /**
   * Metadata key an `allowNoAccess` decorator sets. Defaults to
   * `'adminAllowNoAccess'` — web-platform-backend's existing
   * `@AdminAuth({ allowNoAccess: true })` decorator already writes exactly
   * that key, so the default keeps every existing decorator file wire
   * compatible with zero changes.
   */
  allowNoAccessMetadataKey?: string;
  /**
   * Metadata key a site/feature-scope decorator sets. Defaults to
   * `'adminBackOfficeScope'` for the same reason.
   */
  scopeMetadataKey?: string;
};

export type ResolvedAdminGuardNestOptions = AdminGuardNestOptions &
  Required<
    Pick<AdminGuardNestOptions, 'allowNoAccessMetadataKey' | 'scopeMetadataKey'>
  >;

export type AdminGuardModuleAsyncOptions = {
  imports?: DynamicModule['imports'];
  inject?: FactoryProvider['inject'];
  useFactory: (
    ...args: unknown[]
  ) => AdminGuardNestOptions | Promise<AdminGuardNestOptions>;
};

function resolveOptions(
  options: AdminGuardNestOptions,
): ResolvedAdminGuardNestOptions {
  return {
    ...options,
    allowNoAccessMetadataKey: options.allowNoAccessMetadataKey ?? 'adminAllowNoAccess',
    scopeMetadataKey: options.scopeMetadataKey ?? 'adminBackOfficeScope',
  };
}

/**
 * Provides the two services `AdminAuthGuard` / `AdminBackOfficeFeatureGuard`
 * depend on (`AdminJwtVerifier`, `AdminIntrospectClient`), wired from
 * explicit config rather than `process.env` (this library never reads env
 * itself — the host app resolves values from its own config/env and passes
 * them in via `forRoot`/`forRootAsync`).
 *
 * Deliberately does NOT provide the guard classes themselves — the host app
 * declares those as its own providers (see `AdminAuthGuard`/
 * `AdminBackOfficeFeatureGuard`'s docs), so there is exactly one place that
 * instantiates them and no risk of two independently-DI'd copies.
 *
 * Import AND re-export this module from the host app's own admin-wiring
 * module, the same way the old `AdminIdpModule` had to be re-exported: Nest
 * resolves an enhancer's (guard's) constructor deps from the HOST MODULE OF
 * EACH CONTROLLER, so under-exporting `AdminJwtVerifier`/
 * `AdminIntrospectClient` breaks bootstrap in every module that only
 * imports the host module for guard DI — invisible to a type-check/build,
 * only caught by actually booting.
 */
@Module({})
export class AdminGuardModule {
  static forRoot(options: AdminGuardNestOptions): DynamicModule {
    return AdminGuardModule.build([
      { provide: ADMIN_GUARD_OPTIONS, useValue: resolveOptions(options) },
    ]);
  }

  static forRootAsync(asyncOptions: AdminGuardModuleAsyncOptions): DynamicModule {
    const optionsProvider: Provider = {
      provide: ADMIN_GUARD_OPTIONS,
      useFactory: async (...args: unknown[]) =>
        resolveOptions(await asyncOptions.useFactory(...args)),
      inject: asyncOptions.inject ?? [],
    };
    return AdminGuardModule.build([optionsProvider], asyncOptions.imports);
  }

  private static build(
    optionProviders: Provider[],
    imports: DynamicModule['imports'] = [],
  ): DynamicModule {
    return {
      module: AdminGuardModule,
      imports,
      providers: [
        ...optionProviders,
        {
          provide: AdminJwtVerifier,
          useFactory: (options: ResolvedAdminGuardNestOptions) =>
            new AdminJwtVerifier({
              issuer: options.issuer,
              idpBaseUrl: options.idpBaseUrl,
              jwksPath: options.jwksPath,
            }),
          inject: [ADMIN_GUARD_OPTIONS],
        },
        {
          provide: AdminIntrospectClient,
          useFactory: (options: ResolvedAdminGuardNestOptions) =>
            new AdminIntrospectClient({
              idpBaseUrl: options.idpBaseUrl,
              introspectApiKey: options.introspectApiKey,
              introspectPath: options.introspectPath,
              introspectHeader: options.introspectHeader,
              timeoutMs: options.introspectTimeoutMs,
              breaker: options.breaker,
              clock: options.clock,
            }),
          inject: [ADMIN_GUARD_OPTIONS],
        },
      ],
      exports: [ADMIN_GUARD_OPTIONS, AdminJwtVerifier, AdminIntrospectClient],
    };
  }
}
