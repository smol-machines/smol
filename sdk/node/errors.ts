/** Typed errors for the `smol` SDK.
 *
 *  The native addon reports errors as `Error` objects whose message is
 *  prefixed with a bracketed code, e.g. `"[KVM_UNAVAILABLE] …"`. We parse that
 *  back into a typed hierarchy so callers can branch on `err.code` /`instanceof`.
 */

export class SmolError extends Error {
  readonly code: string;
  constructor(code: string, message: string) {
    super(message);
    this.name = new.target.name;
    this.code = code;
  }
}

/** The active backend can't serve this operation (e.g. volumes on local). */
export class NotSupportedError extends SmolError {
  constructor(message: string) {
    super('NOT_SUPPORTED', message);
  }
}

/** A required configuration value is missing or invalid (a usage error). */
export class InvalidConfigError extends SmolError {
  constructor(message: string) {
    super('INVALID_CONFIG', message);
  }
}

/** A command ran but exited non-zero (raised by `ExecResult.assertSuccess()`). */
export class ExecutionError extends SmolError {
  readonly exitCode: number;
  readonly stdout: string;
  readonly stderr: string;
  constructor(exitCode: number, stdout: string, stderr: string) {
    super('COMMAND_FAILED', `Command exited with code ${exitCode}`);
    this.exitCode = exitCode;
    this.stdout = stdout;
    this.stderr = stderr;
  }
}

const BRACKETED = /^\[([A-Z_]+)\]\s*(.*)$/s;

/** Convert any error thrown by the native addon into a typed `SmolError`. */
export function wrapNativeError(err: unknown): SmolError {
  if (err instanceof SmolError) return err;
  const message = err instanceof Error ? err.message : String(err);
  const m = BRACKETED.exec(message);
  if (m) return new SmolError(m[1], m[2]);
  return new SmolError('SMOLVM_ERROR', message);
}
