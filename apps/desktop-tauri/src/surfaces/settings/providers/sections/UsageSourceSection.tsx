import { useState } from "react";
import type { LocaleKey } from "../../../../i18n/keys";
import { setProviderUsageSource } from "../../../../lib/tauri";
import { usageSourcePolicy } from "./usageSourcePolicy";

interface Props {
  providerId: string;
  currentValue: string | null | undefined;
  t: (key: LocaleKey) => string;
  onChanged: () => void;
}


export function UsageSourceSection({
  providerId,
  currentValue,
  t,
  onChanged,
}: Props) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const policy = usageSourcePolicy(providerId);
  if (!policy) return null;
  const { options } = policy;

  const selected = currentValue ?? "auto";
  const selectedOption = options.find((option) => option.value === selected) ?? options[0];

  const handleSelect = async (value: string) => {
    if (value === selected || busy) return;
    setBusy(true);
    setError(null);
    try {
      await setProviderUsageSource(providerId, value);
      onChanged();
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="provider-detail-section provider-detail-usage-source">
      <h4>{t("UsageSource")}</h4>
      <div role="radiogroup" aria-label={t("UsageSource")} className="provider-detail-segmented">
        {options.map((option) => {
          const isActive = option.value === selected;
          return (
            <button
              key={option.value}
              type="button"
              role="radio"
              aria-checked={isActive}
              disabled={busy}
              className={`provider-detail-segmented__option${isActive ? " is-active" : ""}`}
              onClick={() => void handleSelect(option.value)}
            >
              {option.label}
            </button>
          );
        })}
      </div>
      <p className="provider-detail-helper">{selectedOption.description}</p>
      {error && <p className="provider-detail-error">{error}</p>}
    </section>
  );
}
