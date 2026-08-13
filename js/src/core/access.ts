import type { AdminSiteAccess } from './types';

/**
 * Normalizes `siteAccess` into `{ [site]: feature[] }`. Two shapes arrive on
 * the wire and both must yield the same grants (contract/README.md
 * "Shape normalisation"):
 *
 *   { "zone4-back-office": ["news-management"] }              // current
 *   { "zone4-back-office": { "news-management": "edit" } }    // legacy
 *
 * A value that is neither array nor object contributes no entry at all.
 * Output is deduplicated and sorted per site.
 */
export function normalizeAdminSiteAccess(
  raw: AdminSiteAccess | Record<string, unknown> | undefined,
): AdminSiteAccess {
  if (!raw || typeof raw !== 'object') {
    return {};
  }
  const out: AdminSiteAccess = {};
  for (const [siteId, v] of Object.entries(raw)) {
    if (Array.isArray(v)) {
      out[siteId] = [...new Set(v.map(String))].sort();
    } else if (v && typeof v === 'object' && !Array.isArray(v)) {
      out[siteId] = Object.keys(v as Record<string, unknown>).sort();
    }
  }
  return out;
}

export function adminHasSiteOnToken(
  role: string,
  siteAccess: AdminSiteAccess | undefined,
  siteId: string,
): boolean {
  if (role === 'super_admin') {
    return true;
  }
  return Object.prototype.hasOwnProperty.call(siteAccess ?? {}, siteId);
}

export function adminHasFeatureOnSite(
  role: string,
  siteAccess: AdminSiteAccess | undefined,
  siteId: string,
  feature: string,
): boolean {
  if (role === 'super_admin') {
    return true;
  }
  const list = siteAccess?.[siteId];
  return Array.isArray(list) && list.includes(feature);
}

/** super_admin, or at least one site granted. */
export function adminHasAnySiteAccess(
  role: string,
  siteAccess: AdminSiteAccess,
): boolean {
  return role === 'super_admin' || Object.keys(siteAccess).length > 0;
}
