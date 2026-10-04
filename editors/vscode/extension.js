// Starts `cogito lsp` for .cog files: errors and warnings as you type,
// Format Document, hover documentation, outline, and go to definition.
const vscode = require("vscode");
const { LanguageClient } = require("vscode-languageclient/node");

let client;

function activate(context) {
  const command = vscode.workspace.getConfiguration("cogito").get("path") || "cogito";
  const server = { command, args: ["lsp"] };
  client = new LanguageClient(
    "cogito",
    "Cogito",
    { run: server, debug: server },
    { documentSelector: [{ scheme: "file", language: "cogito" }] }
  );
  context.subscriptions.push({ dispose: () => client && client.stop() });
  return client.start();
}

function deactivate() {
  return client ? client.stop() : undefined;
}

module.exports = { activate, deactivate };
