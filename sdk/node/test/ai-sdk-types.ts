// Compile against the published AI SDK contract without making `ai` a
// runtime dependency of smolmachines/ai-sdk or its generated declarations.
import type { Experimental_SandboxSession } from "ai";
import { createSandboxSession, type SmolSandboxSession } from "../ai-sdk";
import type { Machine } from "../machine";

const session: Experimental_SandboxSession = {} as SmolSandboxSession;
const creator: (machine: Machine) => Experimental_SandboxSession = createSandboxSession;
void session;
void creator;
