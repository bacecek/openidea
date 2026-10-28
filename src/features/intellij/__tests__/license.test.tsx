import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import type { IntellijActiveLicense, IntellijLicenseView } from "@/bindings";

type Handler = (request: string, value: string | null, server: number) => unknown;
let handle: Handler;
const lspIntellijLicensing = vi.fn(async (server: number, request: string, value: string | null) => ({
  status: "ok" as const,
  data: await handle(request, value, server),
}));
const seeded = new Map<number, IntellijLicenseView>();
const lspIntellijLicense = vi.fn(async (server: number) => ({
  status: "ok" as const,
  data: seeded.get(server) ?? null,
}));
const openExternalUrl = vi.fn(async (_url: string) => {});
vi.mock("@/platform/ipc", async (orig) => {
  const actual = await orig<typeof import("@/platform/ipc")>();
  return { ...actual, commands: { ...actual.commands, lspIntellijLicensing, lspIntellijLicense } };
});
vi.mock("@/platform/openUrl", () => ({ openExternalUrl }));

const { emitLocalIdeEvent } = await import("@/platform/ipc");
const { useServers } = await import("@/features/code-insight/servers");
const { provideNotificationService } = await import("@/workbench/contributions");
const { MessagesHost } = await import("@/workbench/messages");
const { InputDialogsHost } = await import("@/workbench/inputDialogs");
const { LicenseStatus, useLicenses } = await import("../license");
const { executeAction } = await import("@/platform/actions");
const { activate } = await import("../index");

interface Note {
  level: string;
  title: string;
  body?: unknown;
  actions?: Array<{ label: string; run: () => void }>;
}
let notes: Note[];
let off: () => void;

const view = (kind: IntellijLicenseView["kind"], patch: Partial<IntellijLicenseView> = {}): IntellijLicenseView => ({
  kind,
  license: null,
  account: { authenticationStatus: "NotAuthenticated", displayName: null, email: null, errorMessage: null },
  subscriptions: [],
  licenseServer: { kind: "none", serverAddress: null, floatingTicketStatus: null, errorMessage: null },
  parkedReason: null,
  graceDeadline: null,
  envLicense: false,
  ...patch,
});

const license = (source: string, patch: Partial<IntellijActiveLicense> = {}): IntellijActiveLicense => ({
  source,
  status: "Valid",
  product: "IntelliJ IDEA Ultimate",
  licensedTo: "Jane Doe",
  validThrough: "2026-12-01",
  daysLeft: null,
  assetId: null,
  serverAddress: null,
  ...patch,
});

const servers = (...ids: Array<[number, string]>) =>
  act(() =>
    useServers.setState({
      servers: new Map(
        ids.map(([id, name]) => [id, { id, name, status: "ready", message: null, languages: ["java"], root: null }]),
      ),
    }),
  );

const push = (v: IntellijLicenseView, server = 1) =>
  act(() => emitLocalIdeEvent({ type: "intellijLicense", server, view: v }));

const settle = () => act(async () => void (await new Promise((r) => setTimeout(r, 20))));

beforeEach(() => {
  notes = [];
  handle = () => null;
  provideNotificationService({ notify: (n) => void notes.push(n as Note) });
  servers([1, "intellij-server"]);
  render(
    <>
      <MessagesHost />
      <InputDialogsHost />
      <LicenseStatus />
    </>,
  );
  off = activate();
});

afterEach(async () => {
  for (let i = 0; i < 5 && screen.queryAllByRole("dialog").length; i++)
    for (const b of screen.queryAllByRole("button", { name: /^(Cancel|Close|OK)$/ })) await userEvent.click(b);
  off();
  useServers.setState({ servers: new Map() });
  useLicenses.setState({ views: new Map(), signInUrl: null });
  seeded.clear();
  localStorage.clear();
  vi.clearAllMocks();
});

describe("IntelliJ license state", () => {
  it("keeps the latest view of this window's servers and seeds servers that appear", async () => {
    push(view("licensed", { license: license("subscription") }));
    push(view("parked"), 7);
    expect(useLicenses.getState().views.get(1)?.kind).toBe("licensed");
    expect(useLicenses.getState().views.has(7)).toBe(false);

    seeded.set(2, view("grace"));
    servers([1, "intellij-server"], [2, "intellij-server"], [3, "jdtls"]);
    await settle();
    expect(lspIntellijLicense.mock.calls.map(([id]) => id)).toEqual([1, 2]);
    expect(useLicenses.getState().views.get(2)?.kind).toBe("grace");

    servers([2, "intellij-server"]);
    expect([...useLicenses.getState().views.keys()]).toEqual([2]);
  });

  it("seeds servers already running when the feature starts", async () => {
    off();
    seeded.set(1, view("licensed", { license: license("trial", { daysLeft: 12 }) }));
    off = activate();
    await settle();
    expect(useLicenses.getState().views.get(1)?.license?.source).toBe("trial");
  });

  it("shows the state in the status bar when it needs attention", () => {
    expect(screen.queryByRole("button", { name: /IntelliJ/ })).toBeNull();
    const cases: Array<[IntellijLicenseView, string]> = [
      [view("licensed", { license: license("trial", { daysLeft: 12 }) }), "IntelliJ trial: 12 days"],
      [view("licensed", { license: license("trial", { daysLeft: 1 }) }), "IntelliJ trial: 1 day"],
      [view("grace"), "IntelliJ license problem"],
      [view("parked"), "IntelliJ: no license"],
      [view("unlicensed"), "IntelliJ: no license"],
      [view("sessionExpired", { license: license("subscription") }), "IntelliJ: sign in again"],
      [view("authenticating"), "IntelliJ: signing in…"],
    ];
    for (const [v, text] of cases) {
      push(v);
      expect(screen.getByRole("button", { name: /IntelliJ/ })).toHaveTextContent(text);
    }
    push(view("licensed", { license: license("subscription") }));
    const quiet = screen.getByRole("button", { name: "IntelliJ license" });
    expect(quiet).toHaveTextContent("");
  });
});

const titles = () => notes.map((n) => n.title);

describe("IntelliJ license notices", () => {
  it("offers activation once per session when a server has no license", () => {
    servers([1, "intellij-server"], [2, "intellij-server"]);
    push(view("licensed", { license: license("subscription") }));
    expect(notes).toEqual([]);
    push(view("unlicensed"), 2);
    push(view("parked"), 1);
    push(view("licensed", { license: license("subscription") }));
    push(view("unlicensed"));
    const offers = notes.filter((n) => n.title === "IntelliJ IDEA's Java and Kotlin server needs a license");
    expect(offers).toHaveLength(1);
    expect(offers[0]).toMatchObject({ level: "warning", sticky: true });
    expect(offers[0]!.actions?.map((a) => a.label)).toEqual(["Activate…"]);
  });

  it("opens sign-in pages only for this window's servers", () => {
    act(() => emitLocalIdeEvent({ type: "openExternalUrl", server: 9, url: "https://account.jetbrains.com/other" }));
    expect(openExternalUrl).not.toHaveBeenCalled();
    act(() => emitLocalIdeEvent({ type: "openExternalUrl", server: 1, url: "https://account.jetbrains.com/a" }));
    expect(openExternalUrl).toHaveBeenCalledWith("https://account.jetbrains.com/a");
  });

  it("warns when the license enters grace, is lost, or the session expires", async () => {
    const { useProject } = await import("@/platform/project");
    useProject.setState({ settings: { general: { dateFormat: { overrideSystem: true, use24Hour: true } } } as never });
    const subscription = license("subscription");
    push(view("licensed", { license: subscription }));
    push(view("grace", { graceDeadline: new Date(2026, 9, 28, 17, 45).getTime() }));
    push(view("grace", { graceDeadline: new Date(2026, 9, 28, 17, 45).getTime() }));
    expect(notes).toEqual([
      expect.objectContaining({
        level: "warning",
        title: "IntelliJ license problem",
        body: "Code insight works until 17:45.",
      }),
    ]);

    push(view("parked"));
    expect(notes.at(-1)).toMatchObject({ level: "error", title: "IntelliJ license lost" });

    notes = [];
    push(view("licensed", { license: subscription }));
    push(view("sessionExpired", { license: subscription }));
    push(view("sessionExpired", { license: subscription }));
    expect(titles()).toEqual(["Your JetBrains Account session expired"]);
    expect(notes[0]!.level).toBe("warning");
    expect(notes[0]!.actions?.map((a) => a.label)).toEqual(["Sign In"]);
    useProject.setState({ settings: null });
  });

  it("reminds of the trial's end at 7 and 1 days left, at most once a day", () => {
    vi.useFakeTimers({ toFake: ["Date"] });
    const trial = (daysLeft: number, validThrough = "2026-11-04") =>
      view("licensed", { license: license("trial", { daysLeft, validThrough }) });
    const day = (d: number) => vi.setSystemTime(new Date(2026, 9, d, 10));
    try {
      day(20);
      push(trial(15));
      expect(notes).toEqual([]);
      day(28);
      push(trial(7));
      push(trial(7), 1);
      expect(titles()).toEqual(["IntelliJ trial ends in 7 days"]);
      expect(notes[0]!.actions?.map((a) => a.label)).toEqual(["Activate…"]);
      day(29);
      push(trial(6));
      vi.setSystemTime(new Date(2026, 10, 3, 10));
      push(trial(1));
      push(trial(1));
      vi.setSystemTime(new Date(2026, 10, 3, 18));
      push(trial(0, "2026-11-30"));
      vi.setSystemTime(new Date(2026, 10, 4, 10));
      push(trial(0));
      expect(titles()).toEqual(["IntelliJ trial ends in 7 days", "IntelliJ trial ends tomorrow"]);
      vi.setSystemTime(new Date(2026, 10, 5, 10));
      push(trial(5, "2026-11-30"));
      expect(titles().at(-1)).toBe("IntelliJ trial ends in 5 days");
    } finally {
      vi.useRealTimers();
    }
  });
});

const requests = () => lspIntellijLicensing.mock.calls.map(([, r, v]) => (v === null ? r : `${r} ${v}`));
const dialog = () => screen.getByRole("dialog");
const click = (name: string | RegExp) => userEvent.click(screen.getByRole("button", { name }));

async function openChooser(v = view("unlicensed")) {
  push(v);
  act(() => notes.find((n) => n.title.includes("needs a license"))!.actions![0]!.run());
  await settle();
}

async function choose(item: string) {
  await userEvent.click(within(dialog()).getByText(item));
  await settle();
}

describe("IntelliJ license dialog", () => {
  it("shows the license, the account and the license server", async () => {
    push(
      view("licensed", {
        license: license("subscription", { validThrough: "2027-01-31" }),
        account: {
          authenticationStatus: "Authenticated",
          displayName: "Jane",
          email: "jane@example.com",
          errorMessage: null,
        },
      }),
    );
    await click("IntelliJ license");
    expect(dialog()).toHaveTextContent("IntelliJ IDEA Ultimate");
    expect(dialog()).toHaveTextContent("JetBrains Account");
    expect(dialog()).toHaveTextContent("Jane Doe");
    expect(dialog()).toHaveTextContent("2027-01-31");
    expect(dialog()).toHaveTextContent("jane@example.com");
    for (const name of ["Change License…", "Remove License", "Sign Out", "Close"])
      expect(within(dialog()).getByRole("button", { name })).toBeInTheDocument();
    await click("Close");

    push(
      view("licensed", {
        license: license("licenseServer", { serverAddress: "https://lic.example.com" }),
        licenseServer: {
          kind: "configured",
          serverAddress: "https://lic.example.com",
          floatingTicketStatus: "Obtained",
          errorMessage: null,
        },
      }),
    );
    await executeAction("intellij.license");
    await settle();
    expect(dialog()).toHaveTextContent("https://lic.example.com");
    expect(within(dialog()).queryByRole("button", { name: "Sign Out" })).toBeNull();
    await click("Close");

    push(view("licensed", { license: license("eap") }));
    await executeAction("intellij.license");
    await settle();
    expect(within(dialog()).queryByRole("button", { name: "Remove License" })).toBeNull();
    await click("Close");

    push(view("parked"));
    await click("IntelliJ: no license");
    expect(within(dialog()).getByRole("button", { name: "Activate…" })).toBeInTheDocument();
    expect(within(dialog()).queryByRole("button", { name: "Remove License" })).toBeNull();
    await click("Activate…");
    await settle();
    expect(dialog()).toHaveTextContent("Start 30-day trial");
  });

  const removal: Array<[string, string[]]> = [
    ["subscription", ["subscriptions/unuseAll", "jba/logout"]],
    ["activationCode", ["activationCode/disable"]],
    ["trial", ["activationCode/disable"]],
    ["productKeyFile", ["activationCode/disable"]],
    ["licenseServer", ["licenseServer/remove"]],
  ];
  it.each(removal)("removes a %s license after confirmation, without a lost notice", async (source, expected) => {
    handle = (request) => {
      if (request !== "jba/logout") push(view("parked", { parkedReason: "LicenseRemoved" }));
      return null;
    };
    push(view("licensed", { license: license(source) }));
    await executeAction("intellij.license");
    await settle();
    await click("Remove License");
    await click("Remove");
    await settle();
    expect(requests()).toEqual(expected);
    expect(titles()).not.toContain("IntelliJ license lost");
  });

  it("refuses to remove a license set by an environment variable", async () => {
    push(view("licensed", { license: license("activationCode"), envLicense: true }));
    await executeAction("intellij.license");
    await settle();
    await click("Remove License");
    await settle();
    expect(dialog()).toHaveTextContent("environment variable");
    expect(requests()).toEqual([]);
  });

  it("signs out of the account", async () => {
    push(
      view("licensed", {
        license: license("subscription"),
        account: { authenticationStatus: "Authenticated", displayName: "Jane", email: null, errorMessage: null },
      }),
    );
    await executeAction("intellij.license");
    await settle();
    await click("Sign Out");
    await settle();
    expect(requests()).toEqual(["jba/logout"]);
  });
});

describe("IntelliJ license activation", () => {
  it("offers only a license server while an environment variable sets the license", async () => {
    await openChooser(view("unlicensed", { envLicense: true }));
    expect(dialog()).toHaveTextContent("environment variable");
    expect(dialog()).not.toHaveTextContent("Start 30-day trial");
    await click("Use a License Server…");
    expect(screen.getByRole("textbox", { name: "License server URL:" })).toBeInTheDocument();
  });

  it("starts the trial and reports a failure", async () => {
    handle = () => ({ isSuccess: false, errorMessage: "  Trial already used  " });
    await openChooser();
    await choose("Start 30-day trial");
    expect(requests()).toEqual(["trial/startAnonymous"]);
    expect(notes.at(-1)).toMatchObject({
      level: "error",
      title: "Couldn't start your trial",
      body: "Trial already used",
    });

    handle = () => {
      push(view("licensed", { license: license("trial", { daysLeft: 30 }) }));
      return { isSuccess: true };
    };
    act(() => notes.find((n) => n.title.includes("needs a license"))!.actions![0]!.run());
    await settle();
    await choose("Start 30-day trial");
    expect(notes.at(-1)).toMatchObject({ level: "info", title: "IntelliJ license is active" });
  });

  function login() {
    let reply!: (value: unknown) => void;
    const pending = new Promise((r) => (reply = r));
    return { pending, reply: (value: unknown) => act(() => reply(value)) };
  }

  it("signs in with the browser and shows the sign-in page's address", async () => {
    const { pending, reply } = login();
    handle = (request) => {
      if (request !== "jba/login") return null;
      push(view("authenticating"));
      return pending;
    };
    await openChooser();
    await choose("Sign in with JetBrains Account");
    expect(dialog()).toHaveTextContent("Finish signing in in your browser");
    act(() =>
      emitLocalIdeEvent({ type: "openExternalUrl", server: 1, url: "https://account.jetbrains.com/login?x=1" }),
    );
    expect(dialog()).toHaveTextContent("https://account.jetbrains.com/login?x=1");
    const writeText = vi.fn(async () => {});
    Object.assign(navigator, { clipboard: { writeText } });
    await click("Copy");
    expect(writeText).toHaveBeenCalledWith("https://account.jetbrains.com/login?x=1");

    push(view("licensed", { license: license("subscription") }));
    reply({ status: "Licensed" });
    await settle();
    expect(screen.queryByRole("dialog")).toBeNull();
    expect(notes.at(-1)).toMatchObject({ level: "info", title: "IntelliJ license is active" });
    expect(requests()).toEqual(["jba/login"]);
  });

  it("cancelling the sign-in signs out and ignores the late reply", async () => {
    const { pending, reply } = login();
    handle = (request) => (request === "jba/login" ? pending : null);
    await openChooser();
    await choose("Sign in with JetBrains Account");
    const before = notes.length;
    await click("Cancel");
    reply({ status: "Failed", errorMessage: "Interrupted" });
    await settle();
    expect(requests()).toEqual(["jba/login", "jba/logout"]);
    expect(notes).toHaveLength(before);
  });

  const subscription = (assetId: string, product: string) => ({
    assetId,
    product,
    licensedTo: "Jane Doe",
    validThrough: "2027-01-31",
    licenseId: null,
  });

  it("activates the account's only subscription when the sign-in finds no license by itself", async () => {
    handle = (request, value) => {
      if (request === "jba/login") {
        push(view("unlicensed", { subscriptions: [subscription("a1", "IntelliJ IDEA Ultimate")] }));
        return { status: "NoSuitableLicense" };
      }
      if (request === "subscriptions/use")
        push(view("licensed", { license: license("subscription", { assetId: value }) }));
      return null;
    };
    await openChooser();
    await choose("Sign in with JetBrains Account");
    expect(requests()).toEqual(["jba/login", "subscriptions/use a1"]);
    expect(notes.at(-1)).toMatchObject({ title: "IntelliJ license is active" });
  });

  it("lets the user pick one of several subscriptions, and signs out when there are none", async () => {
    handle = (request) => {
      if (request === "jba/login") {
        push(
          view("unlicensed", {
            subscriptions: [subscription("a1", "IntelliJ IDEA Ultimate"), subscription("a2", "All Products Pack")],
          }),
        );
        return { status: "NoSuitableLicense" };
      }
      return null;
    };
    await openChooser();
    await choose("Sign in with JetBrains Account");
    await choose("All Products Pack, Jane Doe, valid through 2027-01-31");
    expect(requests()).toEqual(["jba/login", "subscriptions/use a2"]);

    lspIntellijLicensing.mockClear();
    handle = (request) => {
      if (request === "jba/login") push(view("unlicensed"));
      return request === "jba/login" ? { status: "NoSuitableLicense" } : null;
    };
    act(() => notes.find((n) => n.title.includes("needs a license"))!.actions![0]!.run());
    await settle();
    await choose("Sign in with JetBrains Account");
    expect(requests()).toEqual(["jba/login", "jba/logout"]);
    expect(notes.at(-1)).toMatchObject({ level: "warning", title: "No licenses available for this account" });
  });

  it.each([
    [{ status: "Canceled" }, null],
    [{ status: "Failed", errorMessage: "Failed to open the sign-in page" }, null],
    [{ status: "NoLicenses" }, "warning"],
    [{ status: "LicenseCheckFailed" }, "warning"],
    [{ status: "TimedOut" }, "error"],
  ])("maps the sign-in reply %o", async (reply, level) => {
    handle = (request) => (request === "jba/login" ? reply : null);
    await openChooser();
    const before = notes.length;
    await choose("Sign in with JetBrains Account");
    if (level === null) expect(notes).toHaveLength(before);
    else expect(notes.at(-1)!.level).toBe(level);
  });

  it("signs out when the sign-in request fails while still signing in", async () => {
    handle = (request) => {
      if (request !== "jba/login") return null;
      push(view("authenticating"));
      throw new Error("timed out");
    };
    lspIntellijLicensing.mockImplementation(async (server, request, value) => {
      try {
        return { status: "ok" as const, data: await handle(request, value, server) };
      } catch (e) {
        return { status: "error" as const, error: { kind: "timeout", message: String(e) } } as never;
      }
    });
    await openChooser();
    await choose("Sign in with JetBrains Account");
    expect(requests()).toEqual(["jba/login", "jba/logout"]);
    expect(notes.at(-1)).toMatchObject({ level: "error", title: "Couldn't sign in to your JetBrains Account" });
  });

  it("asks for the activation code again when it is rejected", async () => {
    handle = (_request, value) => value === "ABCDEF";
    await openChooser();
    await choose("Enter activation code");
    await userEvent.type(screen.getByRole("textbox"), "AB C\tD");
    await click("OK");
    await settle();
    expect(requests()).toEqual(["activationCode/use ABCD"]);
    expect(dialog()).toHaveTextContent("The activation code was rejected");
    const field = screen.getByRole("textbox");
    expect(field).toHaveValue("ABCD");
    await userEvent.type(field, "EF");
    await click("OK");
    await settle();
    expect(requests()).toEqual(["activationCode/use ABCD", "activationCode/use ABCDEF"]);
    expect(screen.queryByRole("dialog")).toBeNull();
  });

  it("checks the license server URL and reports the server's own error", async () => {
    handle = () => {
      push(
        view("unlicensed", {
          licenseServer: {
            kind: "configured",
            serverAddress: "https://lic.example.com",
            floatingTicketStatus: "NotObtained",
            errorMessage: "No free seats",
          },
        }),
      );
      return { status: "Connected" };
    };
    await openChooser();
    await choose("Use a license server");
    const field = screen.getByRole("textbox", { name: "License server URL:" });
    await userEvent.clear(field);
    await userEvent.type(field, "lic.example.com");
    expect(within(dialog()).getByRole("button", { name: "OK" })).toBeDisabled();
    await userEvent.clear(field);
    await userEvent.type(field, " https://lic.example.com ");
    await click("OK");
    await settle();
    expect(requests()).toEqual(["licenseServer/add https://lic.example.com"]);
    expect(notes.at(-1)).toMatchObject({ level: "error", body: "No free seats" });
  });

  it.each([
    [{ status: "Canceled" }, null],
    [{ status: "TimedOut" }, "Sign-in to the license server timed out"],
    [{ status: "Failed", errorMessage: "Unknown host" }, "Unknown host"],
  ])("maps the license server reply %o", async (reply, body) => {
    handle = () => reply;
    await openChooser();
    await choose("Use a license server");
    await userEvent.type(screen.getByRole("textbox"), "https://lic.example.com");
    const before = notes.length;
    await click("OK");
    await settle();
    if (body === null) expect(notes).toHaveLength(before);
    else expect(JSON.stringify(notes.at(-1))).toContain(body);
  });
});
