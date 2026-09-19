/** Device-local OpenCode Go quota estimates do not establish account-wide pace. */
export function providerAllowsPace(
  providerId: string,
  sourceLabel: string | null | undefined,
): boolean {
  return !(
    providerId === "opencodego" &&
    sourceLabel?.trim().toLowerCase() === "local estimate"
  );
}
