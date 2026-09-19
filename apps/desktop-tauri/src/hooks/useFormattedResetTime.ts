import { useEffect, useState } from "react";
import { useLocale } from "./useLocale";

export type ResetTimeFormatMode = "reset" | "expires";

/** Normalize backend reset descriptions without changing their suffix. */
export function normalizeResetDescription(description: string | null): string | null {
  const trimmed = description?.trim() ?? "";
  if (!trimmed) return null;

  const lowercased = trimmed.toLowerCase();
  if (lowercased === "reset" || lowercased === "resets") {
    return "Resets";
  }
  for (const prefix of ["resets in ", "reset in "]) {
    if (lowercased.startsWith(prefix)) {
      return `Resets in ${trimmed.slice(prefix.length)}`;
    }
  }
  for (const prefix of ["resets ", "reset "]) {
    if (lowercased.startsWith(prefix)) {
      return `Resets ${trimmed.slice(prefix.length)}`;
    }
  }
  return `Resets ${trimmed}`;
}

const absoluteResetFormatter = new Intl.DateTimeFormat(undefined, {
  month: "short",
  day: "numeric",
  hour: "numeric",
  minute: "2-digit",
});

/**
 * Format a provider's reset timestamp for display.
 *
 * When `relative` is true, returns a live countdown string. `mode` selects
 * reset wording ("Resets in …") vs expiry wording ("Next expires in …").
 *
 * When `relative` is false, returns the absolute reset time converted to
 * the user's local timezone via `Intl.DateTimeFormat`.
 *
 * Falls back to `fallback` (typically the backend's `resetDescription`) when
 * `resetsAt` is absent or unparseable.
 */
export function useFormattedResetTime(
  resetsAt: string | null,
  fallback: string | null,
  relative: boolean,
  mode: ResetTimeFormatMode = "reset",
): string | null {
  const { t } = useLocale();
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (!resetsAt) return;
    const id = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => window.clearInterval(id);
  }, [resetsAt, relative]);

  const normalizedFallback =
    mode === "reset" ? normalizeResetDescription(fallback) : fallback?.trim() || null;

  if (!resetsAt) {
    return normalizedFallback;
  }
  const target = Date.parse(resetsAt);
  if (Number.isNaN(target)) {
    return normalizedFallback;
  }

  if (relative) {
    const diffMs = target - now;
    const dueNowKey = mode === "expires" ? "NextExpiresDueNow" : "TrayResetsDueNow";
    if (diffMs <= 0) return t(dueNowKey);
    const totalMinutes = Math.floor(diffMs / 60_000);
    const days = Math.floor(totalMinutes / 1440);
    const hours = Math.floor((totalMinutes % 1440) / 60);
    const minutes = totalMinutes % 60;
    if (days > 0) {
      const key = mode === "expires" ? "NextExpiresInDaysHours" : "ResetsInDaysHours";
      return t(key)
        .replace("{}", String(days))
        .replace("{}", String(hours));
    }
    if (hours === 0) {
      const key = mode === "expires" ? "NextExpiresInMinutes" : "ResetsInMinutes";
      return t(key).replace("{}", String(minutes));
    }
    const key = mode === "expires" ? "NextExpiresInHoursMinutes" : "ResetsInHoursMinutes";
    return t(key)
      .replace("{}", String(hours))
      .replace("{}", String(minutes));
  }

  try {
    return absoluteResetFormatter.format(new Date(target));
  } catch {
    return normalizedFallback;
  }
}
