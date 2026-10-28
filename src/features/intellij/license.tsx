import type { ReactNode } from "react";
import { create } from "zustand";
import type { IntellijLicenseView, IntellijSubscription, ServerId } from "@/bindings";
import { useServers, type ServerEntry } from "@/features/code-insight/servers";
import { registerActions } from "@/platform/actions";
import { formatClock } from "@/platform/dateFormat";
import { tx } from "@/platform/i18n";
import { call, commands, onIdeEvent } from "@/platform/ipc";
import { openExternalUrl } from "@/platform/openUrl";
import { BarButton, Button, copyText, Icon, selectableText, T } from "@/ui";
import { notifications, registerStatusItem } from "@/workbench/contributions";
import { chooseItem, promptInput } from "@/workbench/inputDialogs";
import { ask, confirm, showError, type MessageButton } from "@/workbench/messages";
import { INTELLIJ_SERVER } from "./consent";
import styles from "./consent.module.css";

interface LicenseState {
  views: ReadonlyMap<ServerId, IntellijLicenseView>;
  signInUrl: string | null;
}

export const useLicenses = create<LicenseState>(() => ({ views: new Map(), signInUrl: null }));

const alive = (s: ServerEntry | undefined) =>
  s?.name === INTELLIJ_SERVER && s.status !== "crashed" && s.status !== "stopped";

const ownServer = (id: ServerId) => alive(useServers.getState().servers.get(id));

const ATTENTION: IntellijLicenseView["kind"][] = [
  "parked",
  "unlicensed",
  "sessionExpired",
  "grace",
  "authenticating",
  "licensed",
];

function shown(views: ReadonlyMap<ServerId, IntellijLicenseView>): [ServerId, IntellijLicenseView] | null {
  let best: [ServerId, IntellijLicenseView] | null = null;
  for (const entry of views)
    if (!best || ATTENTION.indexOf(entry[1].kind) < ATTENTION.indexOf(best[1].kind)) best = entry;
  return best;
}

function statusText(view: IntellijLicenseView): string | null {
  switch (view.kind) {
    case "grace":
      return tx("IntelliJ license problem");
    case "parked":
    case "unlicensed":
      return tx("IntelliJ: no license");
    case "sessionExpired":
      return tx("IntelliJ: sign in again");
    case "authenticating":
      return tx("IntelliJ: signing in…");
    case "licensed": {
      const days = view.license?.source === "trial" ? view.license.daysLeft : null;
      if (days === null) return null;
      return days === 1 ? tx("IntelliJ trial: 1 day") : tx`IntelliJ trial: ${days} days`;
    }
  }
}

export function LicenseStatus() {
  const current = shown(useLicenses((s) => s.views));
  if (!current) return null;
  const text = statusText(current[1]);
  return (
    <BarButton
      tooltip="IntelliJ license"
      aria-label={text ?? tx("IntelliJ license")}
      onClick={() => void openLicenseDialog(current[0])}
    >
      {text ?? <Icon name="file-license" size={14} />}
    </BarButton>
  );
}

const KEY = "intellij-license";
const TRIAL_REMINDER = "openidea.intellij.trialReminder";
const FEATURES_WORK = new Set<IntellijLicenseView["kind"]>(["licensed", "sessionExpired", "grace"]);
let removedAt = 0;

const SIGN_IN_PAGE_DECLINED = "Failed to open the sign-in page";
const SERVER_URL = /^https?:\/\/[^\s/$.?#]\S*$/i;
const SIGNED_IN = ["Authenticated", "FailedToProlongateAuthentication"];

const SOURCES: Record<string, string> = {
  trial: "Trial",
  eap: "Early Access Program",
  subscription: "JetBrains Account",
  activationCode: "Activation code",
  licenseServer: "License server",
  toolbox: "JetBrains Toolbox",
  productKeyFile: "Product key file",
};

const REMOVAL: Record<string, string> = {
  subscription: "subscriptions/unuseAll",
  activationCode: "activationCode/disable",
  trial: "activationCode/disable",
  productKeyFile: "activationCode/disable",
  licenseServer: "licenseServer/remove",
};

const viewOf = (server: ServerId) => useLicenses.getState().views.get(server);

function waitForView(server: ServerId, ready: (view: IntellijLicenseView) => boolean): Promise<boolean> {
  return new Promise((resolve) => {
    const check = () => {
      const view = viewOf(server);
      return !!view && ready(view);
    };
    if (check()) return resolve(true);
    const finish = (ok: boolean) => {
      clearTimeout(timer);
      unsubscribe();
      resolve(ok);
    };
    const unsubscribe = useLicenses.subscribe(() => {
      if (check()) finish(true);
    });
    const timer = setTimeout(() => finish(false), 5_000);
  });
}

const request = (server: ServerId, name: string, value: string | null = null) =>
  call(commands.lspIntellijLicensing(server, name, value));

const field = (reply: unknown, key: string): unknown =>
  reply && typeof reply === "object" ? (reply as Record<string, unknown>)[key] : undefined;

const errorText = (reply: unknown) => String(field(reply, "errorMessage") ?? "").trim();

const notify = (level: "info" | "warning" | "error", title: string, body?: string) =>
  notifications().notify({ group: "lsp", level, title, body: body || undefined });

const activated = () => notify("info", "IntelliJ license is active");

const failed = (title: string, e: unknown) => notify("error", title, e instanceof Error ? e.message : String(e));

function Row({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <span className={styles.label}>
        <T>{label}</T>
      </span>
      <span>{children}</span>
    </>
  );
}

function stateText(view: IntellijLicenseView): string | null {
  switch (view.kind) {
    case "grace":
      return view.graceDeadline === null
        ? tx("There is a problem with the license.")
        : tx`There is a problem with the license. Code insight works until ${formatClock(new Date(view.graceDeadline))}.`;
    case "parked":
    case "unlicensed":
      return tx("No active license. Java and Kotlin code insight is off.");
    case "sessionExpired":
      return tx("Your JetBrains Account session expired. Sign in again to keep the license.");
    case "authenticating":
      return tx("Signing in…");
    case "licensed":
      return null;
  }
}

function LicenseBody({ view }: { view: IntellijLicenseView }) {
  const { license, account, licenseServer } = view;
  const state = stateText(view);
  const days = license?.daysLeft;
  const through = license?.validThrough ?? null;
  const name = [account.displayName, account.email].filter(Boolean).join(", ");
  return (
    <div className={styles.body}>
      {state && <p>{state}</p>}
      {view.envLicense && (
        <p>
          <T>The license is set by an environment variable, which takes precedence over licenses activated here.</T>
        </p>
      )}
      <div className={styles.form}>
        {license && (
          <>
            <Row label="Product:">{license.product}</Row>
            <Row label="Source:">{tx(SOURCES[license.source] ?? license.source)}</Row>
            {license.licensedTo && <Row label="Licensed to:">{license.licensedTo}</Row>}
            {(through || days != null) && (
              <Row label="Valid through:">
                {[through, days != null ? (days === 1 ? tx("1 day left") : tx`${days} days left`) : null]
                  .filter(Boolean)
                  .join(", ")}
              </Row>
            )}
          </>
        )}
        {SIGNED_IN.includes(account.authenticationStatus) && name && <Row label="Account:">{name}</Row>}
        {licenseServer.serverAddress && <Row label="License server:">{licenseServer.serverAddress}</Row>}
      </div>
    </div>
  );
}

let dialogOpen = false;

async function openLicenseDialog(server: ServerId) {
  const view = viewOf(server);
  if (!view || dialogOpen) return;
  const active = !!view.license && view.kind !== "parked" && view.kind !== "unlicensed";
  const buttons: MessageButton[] = [
    { id: "activate", label: active ? "Change License…" : "Activate…", primary: !active },
  ];
  if (view.license && REMOVAL[view.license.source]) buttons.push({ id: "remove", label: "Remove License" });
  if (SIGNED_IN.includes(view.account.authenticationStatus)) buttons.push({ id: "signOut", label: "Sign Out" });
  buttons.push({ id: "close", label: "Close", primary: active });
  dialogOpen = true;
  const r = await ask({
    title: "IntelliJ License",
    body: <LicenseBody view={view} />,
    icon: "info",
    width: 480,
    cancel: false,
    buttons,
  }).finally(() => (dialogOpen = false));
  if (r.button === "activate") await openActivation(server);
  else if (r.button === "remove") await removeLicense(server);
  else if (r.button === "signOut") await request(server, "jba/logout").catch((e) => failed("Couldn't sign out", e));
}

async function removeLicense(server: ServerId) {
  const view = viewOf(server);
  const source = view?.license?.source;
  const name = source && REMOVAL[source];
  if (!view || !name) return;
  if (view.envLicense) {
    await showError(
      "Can't Remove License",
      "The license is set by an environment variable. Unset it to remove the license.",
    );
    return;
  }
  const ok = await confirm({
    title: "Remove License",
    message: "Remove the IntelliJ license? Java and Kotlin code insight stops until another license is active.",
    ok: "Remove",
  });
  if (!ok) return;
  removedAt = Date.now();
  try {
    await request(server, name);
    if (source === "subscription") await request(server, "jba/logout");
  } catch (e) {
    failed("Couldn't remove the license", e);
  }
}

const ACTIVATION = [
  "Start 30-day trial",
  "Sign in with JetBrains Account",
  "Enter activation code",
  "Use a license server",
] as const;

async function openActivation(server: ServerId) {
  if (viewOf(server)?.envLicense) {
    const r = await ask({
      title: "Activate IntelliJ License",
      body: "The license is set by an environment variable, which takes precedence over licenses activated here. You can still use a license server.",
      icon: "info",
      buttons: [{ id: "server", label: "Use a License Server…", primary: true }],
    });
    if (r.button === "server") await connectLicenseServer(server);
    return;
  }
  const labels = ACTIVATION.map((a) => tx(a));
  const picked = await chooseItem({ title: "Activate IntelliJ License", items: labels });
  const choice = picked === null ? undefined : ACTIVATION[labels.indexOf(picked)];
  if (choice === "Start 30-day trial") await startTrial(server);
  else if (choice === "Sign in with JetBrains Account") await signIn(server);
  else if (choice === "Enter activation code") await enterCode(server);
  else if (choice === "Use a license server") await connectLicenseServer(server);
}

async function startTrial(server: ServerId) {
  try {
    const reply = await request(server, "trial/startAnonymous");
    if (field(reply, "isSuccess") === true) activated();
    else notify("error", "Couldn't start your trial", errorText(reply));
  } catch (e) {
    failed("Couldn't start your trial", e);
  }
}

function SignInBody() {
  const url = useLicenses((s) => s.signInUrl);
  return (
    <div className={styles.body}>
      <p>
        <T>Finish signing in in your browser.</T>
      </p>
      {url ? (
        <div className={styles.form}>
          <span className={`${selectableText} ${styles.url}`}>{url}</span>
          <Button size="small" onClick={() => void copyText(url)}>
            Copy
          </Button>
        </div>
      ) : (
        <p>
          <T>Opening the sign-in page…</T>
        </p>
      )}
    </div>
  );
}

const subscriptionLabel = (s: IntellijSubscription) =>
  [s.product, s.licensedTo, s.validThrough && tx`valid through ${s.validThrough}`].filter(Boolean).join(", ");

async function activateSubscription(server: ServerId) {
  const subscriptions = viewOf(server)?.subscriptions ?? [];
  if (!subscriptions.length) {
    await request(server, "jba/logout").catch(() => {});
    notify("warning", "No licenses available for this account");
    return;
  }
  let chosen = subscriptions[0]!;
  if (subscriptions.length > 1) {
    const labels = subscriptions.map(subscriptionLabel);
    const picked = await chooseItem({ title: "Choose a Subscription", items: labels });
    if (picked === null) return;
    chosen = subscriptions[labels.indexOf(picked)]!;
  }
  await request(server, "subscriptions/use", chosen.assetId);
  const licensed = (v: IntellijLicenseView) =>
    v.kind === "licensed" && v.license?.source === "subscription" && v.license.assetId === chosen.assetId;
  if (await waitForView(server, licensed)) activated();
  else notify("error", "Couldn't activate the subscription");
}

async function signIn(server: ServerId) {
  useLicenses.setState({ signInUrl: null });
  const finished = new AbortController();
  let cancelled = false;
  void ask({
    title: "JetBrains Account",
    body: <SignInBody />,
    icon: "info",
    buttons: [],
    signal: finished.signal,
  }).then((r) => {
    if (r.button === "dismissed") return;
    cancelled = true;
    void request(server, "jba/logout").catch(() => {});
  });
  let reply: unknown;
  try {
    reply = await request(server, "jba/login");
  } catch (e) {
    finished.abort();
    if (cancelled) return;
    if (viewOf(server)?.kind === "authenticating") void request(server, "jba/logout").catch(() => {});
    failed("Couldn't sign in to your JetBrains Account", e);
    return;
  }
  finished.abort();
  if (cancelled) return;
  const status = field(reply, "status");
  const message = errorText(reply);
  try {
    if (status === "Licensed") activated();
    else if (status === "NoSuitableLicense") await activateSubscription(server);
    else if (status === "NoLicenses") notify("warning", "No licenses available for this account");
    else if (status === "LicenseCheckFailed") notify("warning", "Couldn't check the licenses of this account");
    else if (status === "Canceled" || (status === "Failed" && message === SIGN_IN_PAGE_DECLINED)) return;
    else notify("error", "Couldn't sign in to your JetBrains Account", message);
  } catch (e) {
    failed("Couldn't activate the subscription", e);
  }
}

async function enterCode(server: ServerId) {
  let code: string | undefined;
  for (;;) {
    const input = await promptInput({
      title: "Activation Code",
      label: "Activation code:",
      initial: code,
      hint: code === undefined ? undefined : <T>The activation code was rejected.</T>,
    });
    if (input === null) return;
    code = input.replace(/\s+/g, "");
    let reply: unknown;
    try {
      reply = await request(server, "activationCode/use", code);
    } catch (e) {
      return failed("Couldn't check the activation code", e);
    }
    if (reply === true) return activated();
  }
}

async function connectLicenseServer(server: ServerId) {
  const url = await promptInput({
    title: "License Server",
    label: "License server URL:",
    initial: viewOf(server)?.licenseServer.serverAddress ?? "https://",
    validate: (v) => (SERVER_URL.test(v.trim()) ? null : "Enter a URL that starts with http:// or https://"),
  });
  if (url === null) return;
  const title = "Couldn't connect to the license server";
  const before = viewOf(server);
  const refusal = (v: IntellijLicenseView) =>
    v.licenseServer.kind === "configured" && v.license?.source !== "licenseServer"
      ? v.licenseServer.errorMessage?.trim()
      : null;
  try {
    const reply = await request(server, "licenseServer/add", url);
    const status = field(reply, "status");
    if (status === "Connected") {
      await waitForView(server, (v) => v !== before && (v.license?.source === "licenseServer" || !!refusal(v)));
      const view = viewOf(server);
      const refused = view && refusal(view);
      if (refused) notify("error", title, refused);
      else activated();
    } else if (status === "TimedOut") notify("error", title, tx("Sign-in to the license server timed out."));
    else if (status !== "Canceled") notify("error", title, errorText(reply) || tx("Check the address and try again."));
  } catch (e) {
    failed(title, e);
  }
}

function remindOfTrial(server: ServerId, view: IntellijLicenseView) {
  const license = view.license;
  if (view.kind !== "licensed" || license?.source !== "trial" || license.daysLeft === null) return;
  const days = license.daysLeft;
  const stage = days <= 1 ? 1 : days <= 7 ? 7 : 0;
  if (!stage) return;
  const today = new Date().toDateString();
  let last: { validThrough?: string | null; stage?: number; day?: string } = {};
  try {
    last = JSON.parse(localStorage.getItem(TRIAL_REMINDER) ?? "{}");
  } catch {
    last = {};
  }
  if (last.day === today) return;
  if (last.validThrough === license.validThrough && typeof last.stage === "number" && last.stage <= stage) return;
  localStorage.setItem(TRIAL_REMINDER, JSON.stringify({ validThrough: license.validThrough, stage, day: today }));
  notifications().notify({
    group: "lsp",
    level: stage === 1 ? "warning" : "info",
    title:
      days <= 0
        ? "IntelliJ trial ends today"
        : days === 1
          ? "IntelliJ trial ends tomorrow"
          : `IntelliJ trial ends in ${days} days`,
    key: `${KEY}-trial`,
    actions: [{ label: "Activate…", run: () => void openActivation(server) }],
  });
}

function notifyChange(server: ServerId, prev: IntellijLicenseView | undefined, view: IntellijLicenseView): boolean {
  if (view.kind === "grace" && prev?.kind !== "grace") {
    const deadline = view.graceDeadline;
    notifications().notify({
      group: "lsp",
      level: "warning",
      title: "IntelliJ license problem",
      body: deadline === null ? undefined : tx`Code insight works until ${formatClock(new Date(deadline))}.`,
      key: `${KEY}-grace`,
    });
  } else if (view.kind === "parked" && prev && FEATURES_WORK.has(prev.kind)) {
    if (Date.now() - removedAt < 10_000) return false;
    notifications().notify({
      group: "lsp",
      level: "error",
      title: "IntelliJ license lost",
      key: `${KEY}-lost`,
      actions: [{ label: "Activate…", run: () => void openActivation(server) }],
    });
    return true;
  } else if (view.kind === "sessionExpired" && prev?.kind !== "sessionExpired") {
    notifications().notify({
      group: "lsp",
      level: "warning",
      title: "Your JetBrains Account session expired",
      body: "Sign in again to keep your IntelliJ license.",
      key: `${KEY}-session`,
      actions: [{ label: "Sign In", run: () => void signIn(server) }],
    });
  }
  return false;
}

export function installLicensing(): () => void {
  let offered = false;
  const setView = (server: ServerId, view: IntellijLicenseView) => {
    const prev = useLicenses.getState().views.get(server);
    useLicenses.setState((s) => ({ views: new Map(s.views).set(server, view) }));
    const lost = notifyChange(server, prev, view);
    remindOfTrial(server, view);
    if (view.kind === "licensed") notifications().expire?.(`${KEY}-offer`);
    if (offered || (view.kind !== "parked" && view.kind !== "unlicensed")) return;
    offered = true;
    if (lost) return;
    notifications().notify({
      group: "lsp",
      level: "warning",
      title: "IntelliJ IDEA's Java and Kotlin server needs a license",
      key: `${KEY}-offer`,
      sticky: true,
      actions: [{ label: "Activate…", run: () => void openActivation(server) }],
    });
  };
  const seed = (ids: Iterable<ServerId>) => {
    for (const id of ids) {
      void call(commands.lspIntellijLicense(id))
        .then((view) => {
          if (view && ownServer(id) && !useLicenses.getState().views.has(id)) setView(id, view);
        })
        .catch(() => {});
    }
  };
  const sync = () => {
    const own = [...useServers.getState().servers.values()].filter(alive).map((s) => s.id);
    const views = useLicenses.getState().views;
    if ([...views.keys()].some((id) => !own.includes(id)))
      useLicenses.setState({ views: new Map([...views].filter(([id]) => own.includes(id))) });
    seed(own.filter((id) => !known.has(id)));
    known = new Set(own);
  };
  let known = new Set<ServerId>();
  sync();
  const disposers = [
    registerStatusItem({ id: "intellij.license", align: "right", priority: 6, component: LicenseStatus }),
    registerActions([
      {
        id: "intellij.license",
        title: "IntelliJ License…",
        category: "Tools",
        run: () => {
          const current = shown(useLicenses.getState().views);
          if (current) void openLicenseDialog(current[0]);
          else notify("info", "IntelliJ IDEA's server is not running in this project");
        },
      },
    ]),
    useServers.subscribe(sync),
    onIdeEvent("intellijLicense", (e) => {
      if (ownServer(e.server)) setView(e.server, e.view);
    }),
    onIdeEvent("openExternalUrl", (e) => {
      if (!ownServer(e.server)) return;
      useLicenses.setState({ signInUrl: e.url });
      void openExternalUrl(e.url);
    }),
  ];
  return () => disposers.forEach((d) => d());
}
