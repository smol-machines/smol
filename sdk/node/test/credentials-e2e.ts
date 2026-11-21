/**
 * A running machine's credential bindings and values change without a
 * restart: a binding added live is substituted for a new host, a rotated
 * value is used on the next request, and a removed binding is no longer
 * substituted. postman-echo checks Basic auth, so its status shows which value
 * reached it. Needs KVM (or Apple Silicon) and outbound HTTPS.
 */
import { Machine } from "../index";

const opts = { target: "local" as const, handleSignals: false };
// base64("postman:password"), what postman-echo's /basic-auth accepts.
const GOOD = "cG9zdG1hbjpwYXNzd29yZA==";
let failures = 0;
function check(label: string, ok: boolean, detail = "") {
  if (!ok) failures++;
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  (${detail})` : ""}`);
}

const github = { name: "gh", envVar: "GH_TOKEN", hosts: ["api.github.com"], value: "unused" };
const postman = { name: "pm", envVar: "PM_AUTH", hosts: ["postman-echo.com"] };

async function main() {
  const m = await Machine.create(
    { name: `cred-e2e-${process.pid}`, image: "alpine", resources: { network: true }, credentials: [github] },
    opts,
  );
  // The guest's placeholder for `pm`, or `auth` when given.
  const status = async (auth = "$PM_AUTH") => {
    const r = await m.exec([
      "sh",
      "-c",
      `curl -s -o /dev/null -m 15 -w '%{http_code}' -H "Authorization: Basic ${auth}" https://postman-echo.com/basic-auth`,
    ]);
    return r.stdout.trim();
  };
  try {
    const install = await m.exec(["apk", "add", "-q", "curl"]);
    check("curl installs", install.exitCode === 0, install.stderr.trim().slice(0, 80));

    await m.setCredentials([github, { ...postman, value: GOOD }]);
    check("a binding added while running is substituted", (await status()) === "200");

    await m.updateCredentialValues({ pm: "d3Jvbmc6d3Jvbmc=" });
    check("a rotated value is used on the next request", (await status()) === "401");
    await m.updateCredentialValues({ pm: GOOD });
    check("rotating back restores access", (await status()) === "200");

    await m.setCredentials([github, postman]);
    check("a binding that stays keeps its held value", (await status()) === "200");

    const placeholder = (await m.exec(["sh", "-c", "echo $PM_AUTH"])).stdout.trim();
    await m.setCredentials([github]);
    const env = (await m.exec(["sh", "-c", "echo $PM_AUTH"])).stdout.trim();
    check("a removed binding leaves the guest environment", env === "", env);
    const removed = await status(placeholder);
    check("a removed binding is no longer substituted", /^4\d\d$/.test(removed), removed);

    const refused = await m.updateCredentialValues({ pm: GOOD }).then(
      () => "",
      (e: Error) => e.message,
    );
    check("a value for a removed binding is refused", refused.includes("no credential binding 'pm'"), refused);
  } finally {
    await m.delete().catch(() => {});
  }
  console.log(failures ? `${failures} failed` : "all passed");
  process.exit(failures ? 1 : 0);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
