import * as os from "os";
import * as vscode from "vscode";
import type { Context } from "./context";

const rootSection: string = "cairo1";
const sectionsWithPlaceholders: string[] = ["languageServerPath", "scarbPath"];

export class Config
  implements Pick<vscode.WorkspaceConfiguration, "get" | "has" | "update">
{
  private config: vscode.WorkspaceConfiguration;

  public constructor(private readonly ctx: Context) {
    this.config = vscode.workspace.getConfiguration(rootSection);
    vscode.workspace.onDidChangeConfiguration(
      this.onDidChangeConfiguration,
      this,
      ctx.extension.subscriptions,
    );
  }

  private onDidChangeConfiguration() {
    this.ctx.log.trace("reloading configuration");
    this.config = vscode.workspace.getConfiguration(rootSection);

    // TODO(mkaput): Restart language server if needed.
  }

  get<T>(section: string): T | undefined;
  get<T>(section: string, defaultValue: T): T;
  get(section: string, defaultValue?: unknown): unknown {
    const value = this.config.get(section, defaultValue);
    if (
      typeof value === "string" &&
      sectionsWithPlaceholders.includes(section)
    ) {
      // TODO(mkaput): Attach configs to workspace folders when we'll support multi-root workspaces.
      return replacePathPlaceholders(value, undefined);
    } else {
      return value;
    }
  }

  has(section: string): boolean {
    return this.config.has(section);
  }

  update(
    section: string,
    value: unknown,
    configurationTarget?:
      | boolean
      | vscode.ConfigurationTarget
      | null
      | undefined,
    overrideInLanguage?: boolean | undefined,
  ): Thenable<void> {
    return this.config.update(
      section,
      value,
      configurationTarget,
      overrideInLanguage,
    );
  }
}

function replacePathPlaceholders(
  path: string,
  workspaceFolder: vscode.WorkspaceFolder | undefined,
): string {
  // 1. If there is known workspace folder, replace ${workspaceFolder} with it.
  // 2. If it is undefined, assume the first folder in currently opened workspace.
  //    We could use the currently opened document to detect the correct workspace. However, that would be determined by
  //    the document user has opened on Editor startup. This could lead to unpredictable workspace selection in
  //    practice.
  // 3. If no workspace is opened, replace ${workspaceFolder} with empty string.
  const workspaceFolderPath =
    (workspaceFolder ?? vscode.workspace.workspaceFolders?.[0])?.uri.path ?? "";

  return path
    .replace(/\${workspaceFolder}/g, workspaceFolderPath)
    .replace(/\${userHome}/g, os.homedir());
}
