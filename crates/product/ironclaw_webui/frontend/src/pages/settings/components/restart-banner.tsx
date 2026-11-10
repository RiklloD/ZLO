import { InlineNotice } from "../../../design-system/inline-notice";
import { useT } from "../../../lib/i18n";

export function RestartBanner() {
  const t = useT();

  return (
    <InlineNotice tone="warning" role="alert">
      <p>{t("settings.restartRequired")}</p>
      <p className="mt-1 text-xs text-[var(--v2-text-muted)]">
        {t("settings.restartUnavailable")}
      </p>
    </InlineNotice>
  );
}
