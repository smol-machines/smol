/**
 * Credentials through smolmachines-mcp against the real local engine. The
 * server offers credentials by name (SMOL_MCP_CREDENTIALS) and reads their
 * values from its own environment; the agent attaches them by name at create
 * and changes them on the running machine. postman-echo checks Basic auth, so
 * its status shows whether the real value reached it.
 *
 *   cd smol/sdk/mcp && npm install && npm run test:credentials
 *
 * Requires a host the engine supports and outbound HTTPS.
 */

import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import path from "node:path";

type ToolResult = { content: Array<{ type: string; text?: string }>; isError?: boolean };

let failed = 0;
function check(label: string, ok: boolean, detail = ""): void {
  if (!ok) failed++;
  console.log(`  ${ok ? "✓" : "✗"} ${label}${detail ? ` — ${detail}` : ""}`);
}

// base64("postman:password"), what postman-echo's /basic-auth accepts.
const GOOD = "cG9zdG1hbjpwYXNzd29yZA==";

async function main(): Promise<void> {
  console.log("smolmachines-mcp credentials\n");
  const client = new Client({ name: "credentials", version: "0" });
  await client.connect(
    new StdioClientTransport({
      command: process.execPath,
      args: [path.resolve("dist/index.js")],
      env: {
        ...process.env,
        SMOL_TARGET: "local",
        SMOL_MCP_CREDENTIALS: JSON.stringify([
          { name: "gh", envVar: "SMOL_TEST_GH", hosts: ["api.github.com"] },
          { name: "pm", envVar: "SMOL_TEST_PM", hosts: ["postman-echo.com"] },
        ]),
        SMOL_TEST_GH: "unused",
        SMOL_TEST_PM: GOOD,
      } as Record<string, string>,
      stderr: "inherit",
    }),
  );
  const call = async (name: string, args: Record<string, unknown>) => {
    const result = (await client.callTool({ name, arguments: args })) as ToolResult;
    return { result, body: result.content.find((c) => c.type === "text")?.text ?? "" };
  };

  const tools = (await client.listTools()).tools;
  const described = JSON.stringify(tools);
  check("smol_credentials is registered", tools.some((t) => t.name === "smol_credentials"));
  check(
    "tools name the offered credentials and their hosts, never a value",
    described.includes("pm (postman-echo.com)") && !described.includes(GOOD),
  );

  const name = `mcp-cred-${Date.now().toString(36)}`;
  try {
    const created = await call("smol_create", { name, image: "alpine", credentials: ["gh"], persistent: false });
    check("a machine is created with an offered credential", !created.result.isError, created.body);
    const refused = await call("smol_credentials", { machine: name, credentials: ["stripe"] });
    check("a credential the server does not offer is refused", !!refused.result.isError && refused.body.includes("offered: gh, pm"), refused.body);

    await call("smol_exec", { machine: name, command: "apk add -q curl" });
    const status = async (auth = "$PM_AUTH") => {
      const r = await call("smol_exec", {
        machine: name,
        command: `curl -s -o /dev/null -m 15 -w '%{http_code}' -H "Authorization: Basic ${auth}" https://postman-echo.com/basic-auth`,
      });
      return JSON.parse(r.body).stdout.trim() as string;
    };
    const placeholderOf = async () =>
      JSON.parse((await call("smol_exec", { machine: name, command: "echo $SMOL_TEST_PM" })).body).stdout.trim() as string;

    const added = await call("smol_credentials", { machine: name, credentials: ["gh", "pm"] });
    check("a credential is attached to the running machine", !added.result.isError, added.body);
    const placeholder = await placeholderOf();
    check("the machine sees a placeholder, not the value", placeholder.startsWith("SMOL_PLACEHOLDER_") && placeholder !== GOOD, placeholder);
    const ok = await status("$SMOL_TEST_PM");
    check("the real value reaches the credential's host", ok === "200", ok);

    await call("smol_credentials", { machine: name, credentials: ["gh"] });
    const removed = await status(placeholder);
    check("a credential left out stops being substituted", /^4\d\d$/.test(removed), removed);
  } finally {
    await call("smol_delete", { machine: name }).catch(() => {});
    await client.close();
  }
  console.log(failed ? `\n${failed} failed` : "\nall passed");
  process.exit(failed ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
