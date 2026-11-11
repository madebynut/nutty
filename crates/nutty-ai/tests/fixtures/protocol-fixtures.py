"""Isolated JSON-RPC fixtures used by Nutty's protocol tests."""
import json
import sys

mode = sys.argv[1]
authenticated = False
model = "other"
effort = "high"

def configuration():
    return [
        {"id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": model,
         "options": [{"group": "cursor", "name": "Cursor Models", "options": [{"value": "auto", "name": "Auto"}, {"value": "grok-4.7", "name": "Grok 4.7"}]},
                     {"group": "other", "name": "Other Models", "options": [{"value": "other", "name": "Other model"}, {"value": "rejected", "name": "Unavailable model"}]}]},
        {"id": "effort", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": effort,
         "options": [{"value": "low", "name": "Low"}, {"value": "high", "name": "High"}]},
    ]

def send(message):
    print(json.dumps(message), flush=True)

for line in sys.stdin:
    request = json.loads(line)
    method = request.get("method")
    identity = request.get("id")
    if identity is None:
        continue
    if method == "initialize":
        if mode == "acp-startup-wait":
            import pathlib, os, time
            marker = pathlib.Path(sys.argv[2])
            pending = marker.with_suffix(".tmp")
            pending.write_text(str(os.getpid()))
            # Publish readiness only once the PID is complete.
            pending.replace(marker)
            time.sleep(60)
            continue
        result = {"protocolVersion": request["params"]["protocolVersion"]}
        if mode == "acp-config":
            assert request["params"]["clientCapabilities"]["_meta"]["parameterizedModelPicker"] is True
        if mode == "mcp":
            result.update(capabilities={"tools": {}}, serverInfo={"name": "fixture", "version": "1"})
        else:
            result.update(agentCapabilities={}, agentInfo={"name": "fixture", "version": "1"})
            if mode in ("acp-cursor", "acp-login"):
                assert request["params"]["clientInfo"]["name"] == "nutty"
                result["authMethods"] = [{"id": "cursor_login", "name": "Cursor Login"}]
            elif mode in ("acp-claude", "acp-claude-login"):
                result["authMethods"] = [{"id": "claude-login", "name": "Claude terminal login"}]
            elif mode == "acp-gemini":
                result["authMethods"] = [{"id": "oauth-personal", "name": "Google login"}]
    elif method == "authenticate":
        assert mode in ("acp-cursor", "acp-login", "acp-gemini"), "terminal login must not call authenticate"
        assert request["params"]["methodId"] == ("oauth-personal" if mode == "acp-gemini" else "cursor_login")
        if mode == "acp-login":
            send({"jsonrpc": "2.0", "id": identity, "error": {"code": -32000, "message": "Authentication required", "data": {"secret": "fixture-secret-must-not-leak"}}})
            continue
        authenticated = True
        result = {}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "Echo fixture data", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": request["params"]["arguments"]["text"]}]}
    elif method == "session/new":
        if len(sys.argv) > 2 and sys.argv[2] == "ui":
            import time
            time.sleep(2)

        if mode == "acp-claude-login" or mode in ("acp-cursor", "acp-gemini") and not authenticated:
            send({"jsonrpc": "2.0", "id": identity, "error": {"code": -32000, "message": "Authentication required"}})
            continue
        result = {"sessionId": "fixture-session"}
        if mode == "acp-config":
            result["configOptions"] = configuration()
        elif mode == "acp-models-legacy":
            result["models"] = {"currentModelId": model, "availableModels": [{"modelId": "other", "name": "Other model"}, {"modelId": "auto", "name": "Auto"}, {"modelId": "grok-4.7", "name": "Grok 4.7"}]}
    elif method == "session/set_config_option":
        assert mode == "acp-config"
        assert request["params"]["sessionId"] == "fixture-session"
        value = request["params"]["value"]
        if value == "rejected":
            send({"jsonrpc": "2.0", "id": identity, "error": {"code": -32602, "message": "Unsupported model", "data": {"secret": "fixture-secret-must-not-leak"}}})
            continue
        if request["params"]["configId"] == "model":
            model = value
        else:
            effort = value
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "other-session", "update": {"sessionUpdate": "config_option_update", "configOptions": []}}})
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "config_option_update", "configOptions": configuration()}}})
        result = {"configOptions": configuration()}
    elif method == "session/set_model":
        assert mode == "acp-models-legacy"
        assert request["params"]["sessionId"] == "fixture-session"
        model = request["params"]["modelId"]
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "current_model_update", "currentModelId": model}}})
        result = {}
    elif method == "session/prompt":
        if mode == "acp-operations":
            text = request["params"]["prompt"][0]["text"]
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
            send({"jsonrpc": "2.0", "id": identity, "result": {"stopReason": "end_turn"}})
            continue
        if len(sys.argv) > 2 and sys.argv[2] == "ui":
            import time
            time.sleep(3)
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "检查当前目录：\n\n```bash\npwd\n```\n"}}}})
            send({"jsonrpc": "2.0", "id": identity, "result": {"stopReason": "end_turn"}})
            continue

        if mode in ("acp-config", "acp-models-legacy"):
            if model == "other":
                send({"jsonrpc": "2.0", "id": identity, "error": {"code": -32000, "message": "Model quota exceeded"}})
                continue
            send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": model + ":" + effort}}}})
            send({"jsonrpc": "2.0", "id": identity, "result": {"stopReason": "end_turn"}})
            continue
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Investigating. "}}}})
        send({"jsonrpc": "2.0", "id": "permission", "method": "session/request_permission", "params": {"sessionId": "fixture-session", "toolCall": {"toolCallId": "tool", "title": "Fixture command", "rawInput": {"command": "pwd"}}, "options": [{"optionId": "allow", "name": "Allow once", "kind": "allow_once"}, {"optionId": "reject", "name": "Reject once", "kind": "reject_once"}]}})
        decision = json.loads(next(sys.stdin))["result"]["outcome"]
        text = "Decision: " + decision.get("optionId", "cancelled")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "fixture-session", "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})
        result = {"stopReason": "end_turn"}
    else:
        send({"jsonrpc": "2.0", "id": identity, "error": {"code": -32601, "message": "Unsupported fixture method"}})
        continue
    send({"jsonrpc": "2.0", "id": identity, "result": result})
