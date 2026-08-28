import { VERSION } from "../version.ts";
import { runDoctor, renderDoctor } from "../doctor.ts";
import { BunCommandRunner } from "../process/command.ts";

const HELP = `altitude ${VERSION}

An intent control plane for delegated agentic development.

Usage:
  altitude [command]

Commands:
  doctor     Check the local Altitude runtime boundary
  help       Show this help
  version    Print the Altitude version
`;

export async function runCli(args: readonly string[]): Promise<number> {
  const command = args[0] ?? "help";

  switch (command) {
    case "help":
    case "--help":
    case "-h":
      process.stdout.write(HELP);
      return 0;
    case "version":
    case "--version":
    case "-v":
      process.stdout.write(`${VERSION}\n`);
      return 0;
    case "doctor": {
      const findings = await runDoctor(new BunCommandRunner());
      process.stdout.write(renderDoctor(findings));
      return findings.some((finding) => finding.severity === "error") ? 1 : 0;
    }
    default:
      process.stderr.write(`Unknown command: ${command}\n\n${HELP}`);
      return 2;
  }
}
