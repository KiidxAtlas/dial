// Dial's bridge to the on-device Apple foundation model (FoundationModels framework).
//
// One request per process; Dial cancels a request by killing the process.
//
//   dial-apple-fm availability   -> one JSON line: {"type":"availability", ...}
//   dial-apple-fm chat           -> reads one JSON request from stdin until EOF, writes JSON-line events
//
// The chat request uses OpenAI Chat Completions shapes: `messages`, `tools`, `tool_choice`,
// `response_format` (json_schema), `max_tokens`, `temperature`. Events, one JSON object per line:
//   {"type":"text","delta":"..."}
//   {"type":"tool_call","id":"call_...","name":"...","arguments":"{...}"}
//   {"type":"done","finish_reason":"stop|tool_calls|length","usage":{...}}   (usage only when known)
//   {"type":"error","kind":"context_exceeded|guardrail|refusal|unavailable|rate_limited|busy|
//                          unsupported_language|unsupported|timeout|request|other","message":"..."}
// Every successful run ends with exactly one `done` or `error` event. The Rust side lives in
// src/llm/apple.rs; bump `protocolVersion` together with it when the wire format changes.

import Foundation
import FoundationModels

let protocolVersion = 1

// MARK: - Output

func emit(_ object: [String: Any]) {
    guard
        var data = try? JSONSerialization.data(
            withJSONObject: object, options: [.withoutEscapingSlashes, .sortedKeys])
    else { return }
    data.append(0x0A)
    FileHandle.standardOutput.write(data)
}

func emitError(_ kind: String, _ message: String) {
    emit(["type": "error", "kind": kind, "message": message])
}

// MARK: - Tools

/// A tool call is not executed here: the model's request is captured and handed to Dial, which runs
/// the tool and sends the output back in the next request's transcript.
struct CapturedCall: Error {
    let name: String
    let arguments: String
}

struct RequestError: Error {
    let message: String
}

struct ForwardedTool: Tool {
    let name: String
    let description: String
    let parameters: GenerationSchema
    /// Properties that could not be expressed as a generation schema are generated as JSON text;
    /// these paths are decoded back into JSON values before the call reaches Dial.
    let encodedPaths: [[String]]

    func call(arguments: GeneratedContent) async throws -> String {
        throw CapturedCall(name: name, arguments: decodeEncoded(arguments.jsonString, encodedPaths))
    }
}

/// Path element `*` stands for every element of an array.
func decodeEncoded(_ json: String, _ paths: [[String]]) -> String {
    guard !paths.isEmpty,
        let data = json.data(using: .utf8),
        var value = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
    else { return json }
    for path in paths {
        value = decodePath(value, path[...])
    }
    guard
        let out = try? JSONSerialization.data(
            withJSONObject: value, options: [.withoutEscapingSlashes, .fragmentsAllowed]),
        let text = String(data: out, encoding: .utf8)
    else { return json }
    return text
}

func decodePath(_ value: Any, _ path: ArraySlice<String>) -> Any {
    guard let head = path.first else {
        if let text = value as? String, let data = text.data(using: .utf8),
            let decoded = try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
        {
            return decoded
        }
        return value
    }
    let rest = path.dropFirst()
    if head == "*", let array = value as? [Any] {
        return array.map { decodePath($0, rest) }
    }
    if var object = value as? [String: Any], let child = object[head] {
        object[head] = decodePath(child, rest)
        return object
    }
    return value
}

// MARK: - JSON Schema -> generation schema

/// Converts the JSON Schema subset tool and response schemas use. Anything it cannot express
/// becomes a string the model fills with JSON text, recorded in `encodedPaths`.
final class SchemaBuilder {
    private let prefix: String
    private let definitions: [String: Any]
    private var counter = 0
    private(set) var encodedPaths: [[String]] = []

    init(prefix: String, root: [String: Any]) {
        self.prefix = prefix
        definitions =
            (root["$defs"] as? [String: Any]) ?? (root["definitions"] as? [String: Any]) ?? [:]
    }

    private func name(_ hint: String) -> String {
        counter += 1
        return "\(prefix)_\(hint)_\(counter)"
    }

    /// The root must be an object; a missing or empty schema means "no arguments".
    func root(_ schema: [String: Any]) throws -> GenerationSchema {
        let built = object(schema, hint: "arguments", path: [], depth: 0)
        return try GenerationSchema(root: built, dependencies: [])
    }

    private func object(_ schema: [String: Any], hint: String, path: [String], depth: Int)
        -> DynamicGenerationSchema
    {
        let properties = schema["properties"] as? [String: Any] ?? [:]
        let required = schema["required"] as? [String] ?? []
        // JSON objects lose their key order in Foundation; required keys keep the schema's order.
        let ordered =
            required.filter { properties[$0] != nil }
            + properties.keys.filter { !required.contains($0) }.sorted()
        let fields = ordered.map { key -> DynamicGenerationSchema.Property in
            let child = properties[key] as? [String: Any] ?? [:]
            let (schema, nullable) = build(child, hint: key, path: path + [key], depth: depth + 1)
            return DynamicGenerationSchema.Property(
                name: key,
                description: describe(child, encoded: encodedPaths.last == path + [key]),
                schema: schema,
                isOptional: nullable || !required.contains(key))
        }
        return DynamicGenerationSchema(
            name: name(hint), description: schema["description"] as? String, properties: fields)
    }

    private func describe(_ schema: [String: Any], encoded: Bool) -> String? {
        let base = schema["description"] as? String
        guard encoded else { return base }
        return [base, "Write this value as JSON text."].compactMap { $0 }.joined(separator: " ")
    }

    private func encoded(_ path: [String]) -> DynamicGenerationSchema {
        encodedPaths.append(path)
        return DynamicGenerationSchema(type: String.self)
    }

    private func resolve(_ schema: [String: Any]) -> [String: Any] {
        guard let reference = schema["$ref"] as? String else { return schema }
        for marker in ["#/$defs/", "#/definitions/"] where reference.hasPrefix(marker) {
            let key = String(reference.dropFirst(marker.count))
            if var target = definitions[key] as? [String: Any] {
                if let description = schema["description"] { target["description"] = description }
                return target
            }
        }
        return [:]
    }

    /// Returns the schema and whether `null` is an accepted value.
    private func build(_ raw: [String: Any], hint: String, path: [String], depth: Int)
        -> (DynamicGenerationSchema, Bool)
    {
        guard depth <= 12 else { return (encoded(path), false) }
        let schema = resolve(raw)
        if schema.isEmpty, raw["$ref"] != nil { return (encoded(path), false) }

        if let values = schema["enum"] as? [Any] {
            let nullable = values.contains { $0 is NSNull }
            let strings = values.compactMap { $0 as? String }
            guard !strings.isEmpty, strings.count == values.count - (nullable ? 1 : 0) else {
                return (encoded(path), nullable)
            }
            return (
                DynamicGenerationSchema(
                    name: name(hint), description: schema["description"] as? String,
                    anyOf: strings), nullable
            )
        }
        if let constant = schema["const"] as? String {
            return (DynamicGenerationSchema(name: name(hint), anyOf: [constant]), false)
        }
        if let variants = (schema["anyOf"] ?? schema["oneOf"]) as? [[String: Any]] {
            let nullable = variants.contains { $0["type"] as? String == "null" }
            let rest = variants.filter { $0["type"] as? String != "null" }
            if rest.count == 1 {
                let (only, innerNull) = build(rest[0], hint: hint, path: path, depth: depth + 1)
                return (only, nullable || innerNull)
            }
            // Choices between shapes cannot be decoded back to a field path reliably.
            return (encoded(path), nullable)
        }
        if let parts = schema["allOf"] as? [[String: Any]], parts.count == 1 {
            return build(parts[0], hint: hint, path: path, depth: depth + 1)
        }

        var types: [String]
        if let type = schema["type"] as? String {
            types = [type]
        } else if let list = schema["type"] as? [String] {
            types = list
        } else {
            types = schema["properties"] != nil ? ["object"] : []
        }
        let nullable = types.contains("null")
        types.removeAll { $0 == "null" }
        guard types.count == 1 else { return (encoded(path), nullable) }

        switch types[0] {
        case "string":
            return (DynamicGenerationSchema(type: String.self), nullable)
        case "integer":
            return (DynamicGenerationSchema(type: Int.self), nullable)
        case "number":
            return (DynamicGenerationSchema(type: Double.self), nullable)
        case "boolean":
            return (DynamicGenerationSchema(type: Bool.self), nullable)
        case "array":
            guard let items = schema["items"] as? [String: Any] else {
                return (encoded(path), nullable)
            }
            let (element, _) = build(items, hint: hint + "_item", path: path + ["*"], depth: depth + 1)
            return (
                DynamicGenerationSchema(
                    arrayOf: element,
                    minimumElements: schema["minItems"] as? Int,
                    maximumElements: schema["maxItems"] as? Int), nullable
            )
        case "object":
            // A map with free-form keys has no fixed properties to generate.
            if (schema["properties"] as? [String: Any] ?? [:]).isEmpty,
                schema["additionalProperties"] != nil
            {
                return (encoded(path), nullable)
            }
            return (object(schema, hint: hint, path: path, depth: depth), nullable)
        default:
            return (encoded(path), nullable)
        }
    }
}

// MARK: - Request

func text(of content: Any?) -> String {
    if let text = content as? String { return text }
    if let parts = content as? [[String: Any]] {
        return parts.compactMap { part in
            part["type"] as? String == "text" ? part["text"] as? String : nil
        }.joined(separator: "\n")
    }
    return ""
}

func segments(_ text: String) -> [Transcript.Segment] {
    [.text(Transcript.TextSegment(content: text))]
}

struct Prepared {
    var entries: [Transcript.Entry]
    var prompt: String
    var tools: [ForwardedTool]
    var mode: String
}

func makeTools(_ specs: [[String: Any]]) throws -> [ForwardedTool] {
    try specs.enumerated().compactMap { index, spec in
        guard let function = spec["function"] as? [String: Any],
            let name = function["name"] as? String
        else { return nil }
        let parameters = function["parameters"] as? [String: Any] ?? [:]
        let builder = SchemaBuilder(prefix: "tool\(index)", root: parameters)
        let schema: GenerationSchema
        do {
            schema = try builder.root(parameters)
        } catch {
            throw RequestError(message: "tool \(name): unsupported parameter schema: \(error)")
        }
        return ForwardedTool(
            name: name,
            description: function["description"] as? String ?? "",
            parameters: schema,
            encodedPaths: builder.encodedPaths)
    }
}

/// Rebuilds the conversation as a transcript. Leading system messages become the instructions;
/// later ones become prompts so they keep their place. A final user message is the prompt to answer;
/// a conversation that ends with tool output continues with an empty prompt.
func prepare(_ request: [String: Any]) throws -> Prepared {
    guard let messages = request["messages"] as? [[String: Any]] else {
        throw RequestError(message: "request has no messages array")
    }
    var tools = try makeTools(request["tools"] as? [[String: Any]] ?? [])
    var mode = "auto"
    if let choice = request["tool_choice"] as? String {
        mode = choice
    } else if let choice = request["tool_choice"] as? [String: Any],
        let name = (choice["function"] as? [String: Any])?["name"] as? String
    {
        tools = tools.filter { $0.name == name }
        mode = "required"
    }

    var instructions: [String] = []
    var entries: [Transcript.Entry] = []
    var callNames: [String: String] = [:]
    var leading = true
    for message in messages {
        let role = message["role"] as? String ?? ""
        let body = text(of: message["content"])
        switch role {
        case "system", "developer":
            if leading {
                instructions.append(body)
            } else {
                entries.append(.prompt(Transcript.Prompt(segments: segments(body))))
            }
            continue
        case "user":
            entries.append(.prompt(Transcript.Prompt(segments: segments(body))))
        case "assistant":
            if !body.isEmpty {
                entries.append(.response(Transcript.Response(assetIDs: [], segments: segments(body))))
            }
            let calls = (message["tool_calls"] as? [[String: Any]] ?? []).compactMap {
                call -> Transcript.ToolCall? in
                guard let id = call["id"] as? String,
                    let function = call["function"] as? [String: Any],
                    let name = function["name"] as? String
                else { return nil }
                callNames[id] = name
                let raw = function["arguments"] as? String ?? "{}"
                let arguments =
                    (try? GeneratedContent(json: raw)) ?? (try? GeneratedContent(json: "{}"))
                guard let arguments else { return nil }
                return Transcript.ToolCall(id: id, toolName: name, arguments: arguments)
            }
            if !calls.isEmpty {
                entries.append(.toolCalls(Transcript.ToolCalls(id: UUID().uuidString, calls)))
            }
        case "tool":
            let id = message["tool_call_id"] as? String ?? ""
            entries.append(
                .toolOutput(
                    Transcript.ToolOutput(
                        id: id, toolName: callNames[id] ?? "tool", segments: segments(body))))
        default:
            throw RequestError(message: "unsupported message role \(role)")
        }
        leading = false
    }

    var prompt = ""
    if case .prompt(let last)? = entries.last {
        entries.removeLast()
        prompt = last.segments.compactMap { segment -> String? in
            if case .text(let text) = segment { return text.content }
            return nil
        }.joined(separator: "\n")
    }
    let instructionText = instructions.joined(separator: "\n\n")
    if !instructionText.isEmpty || !tools.isEmpty {
        entries.insert(
            .instructions(
                Transcript.Instructions(
                    segments: segments(instructionText),
                    toolDefinitions: tools.map { Transcript.ToolDefinition(tool: $0) })),
            at: 0)
    }
    return Prepared(entries: entries, prompt: prompt, tools: tools, mode: mode)
}

func options(_ request: [String: Any], mode: String) -> GenerationOptions {
    let temperature = request["temperature"] as? Double
    let maxTokens = (request["max_tokens"] as? Int).flatMap { $0 > 0 ? $0 : nil }
    var options = GenerationOptions(
        temperature: temperature == 0 ? nil : temperature, maximumResponseTokens: maxTokens)
    if temperature == 0 {
        options.samplingMode = .greedy
    }
    if #available(macOS 27.0, *) {
        switch mode {
        case "none": options.toolCallingMode = .disallowed
        case "required": options.toolCallingMode = .required
        default: break
        }
    }
    return options
}

// MARK: - Errors

func classify(_ error: Error) -> String {
    if let error = error as? LanguageModelSession.GenerationError {
        switch error {
        case .exceededContextWindowSize: return "context_exceeded"
        case .guardrailViolation: return "guardrail"
        case .refusal: return "refusal"
        case .assetsUnavailable: return "unavailable"
        case .rateLimited: return "rate_limited"
        case .concurrentRequests: return "busy"
        case .unsupportedLanguageOrLocale: return "unsupported_language"
        case .unsupportedGuide, .decodingFailure: return "unsupported"
        @unknown default: return "other"
        }
    }
    if #available(macOS 27.0, *), let error = error as? LanguageModelError {
        switch error {
        case .contextSizeExceeded: return "context_exceeded"
        case .guardrailViolation: return "guardrail"
        case .refusal: return "refusal"
        case .rateLimited: return "rate_limited"
        case .timeout: return "timeout"
        case .unsupportedLanguageOrLocale: return "unsupported_language"
        case .unsupportedCapability, .unsupportedTranscriptContent, .unsupportedGenerationGuide:
            return "unsupported"
        @unknown default: return "other"
        }
    }
    return "other"
}

func describe(_ error: Error) -> String {
    let localized = error as? LocalizedError
    let parts = [localized?.errorDescription, localized?.failureReason]
        .compactMap { $0 }.filter { !$0.isEmpty }
    return parts.isEmpty ? String(describing: error) : parts.joined(separator: " ")
}

// MARK: - Commands

func availability() {
    let model = SystemLanguageModel.default
    var event: [String: Any] = [
        "type": "availability", "protocol": protocolVersion, "context_size": model.contextSize,
    ]
    switch model.availability {
    case .available:
        event["available"] = true
    case .unavailable(let reason):
        event["available"] = false
        switch reason {
        case .deviceNotEligible: event["reason"] = "device_not_eligible"
        case .appleIntelligenceNotEnabled: event["reason"] = "apple_intelligence_not_enabled"
        case .modelNotReady: event["reason"] = "model_not_ready"
        @unknown default: event["reason"] = "unknown"
        }
    }
    emit(event)
}

func chat() async {
    let request: [String: Any]
    do {
        let data = FileHandle.standardInput.readDataToEndOfFile()
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw RequestError(message: "request is not a JSON object")
        }
        request = object
    } catch {
        emitError("request", "invalid request: \(error)")
        return
    }

    let prepared: Prepared
    let schema: GenerationSchema?
    do {
        prepared = try prepare(request)
        schema = try responseSchema(request)
    } catch let error as RequestError {
        emitError("request", error.message)
        return
    } catch {
        emitError("request", describe(error))
        return
    }

    // Dial sends code, diffs and logs; the default guardrails reject too much of that as input.
    let model = SystemLanguageModel(useCase: .general, guardrails: .permissiveContentTransformations)
    let tools: [any Tool] = (prepared.mode == "none" && !isToolModeAvailable()) ? [] : prepared.tools
    let session = LanguageModelSession(
        model: model, tools: tools, transcript: Transcript(entries: prepared.entries))
    let generation = options(request, mode: prepared.mode)
    let maxTokens = request["max_tokens"] as? Int ?? 0

    do {
        if let schema {
            let response = try await session.respond(
                to: prepared.prompt, schema: schema, includeSchemaInPrompt: true,
                options: generation)
            emit(["type": "text", "delta": response.content.jsonString])
        } else {
            var sent = String.UnicodeScalarView()
            for try await snapshot in session.streamResponse(
                to: prepared.prompt, options: generation)
            {
                let full = snapshot.content.unicodeScalars
                guard full.starts(with: sent) else { continue }
                let delta = String(String.UnicodeScalarView(full.dropFirst(sent.count)))
                if !delta.isEmpty {
                    emit(["type": "text", "delta": delta])
                }
                sent = full
            }
        }
        finish(session, reason: "stop", maxTokens: maxTokens)
    } catch let error as LanguageModelSession.ToolCallError {
        guard let call = error.underlyingError as? CapturedCall else {
            emitError("other", describe(error))
            return
        }
        emit([
            "type": "tool_call",
            "id": "call_" + UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(24),
            "name": call.name,
            "arguments": call.arguments,
        ])
        finish(session, reason: "tool_calls", maxTokens: 0)
    } catch {
        emitError(classify(error), describe(error))
    }
}

func isToolModeAvailable() -> Bool {
    if #available(macOS 27.0, *) { return true }
    return false
}

func responseSchema(_ request: [String: Any]) throws -> GenerationSchema? {
    guard let format = request["response_format"] as? [String: Any],
        format["type"] as? String == "json_schema",
        let wrapper = format["json_schema"] as? [String: Any],
        let schema = wrapper["schema"] as? [String: Any]
    else { return nil }
    let builder = SchemaBuilder(prefix: wrapper["name"] as? String ?? "response", root: schema)
    guard (schema["type"] as? String ?? "object") == "object" else {
        throw RequestError(message: "response_format schema must be an object")
    }
    let built = try builder.root(schema)
    guard builder.encodedPaths.isEmpty else {
        throw RequestError(
            message: "response_format schema uses constructs the on-device model cannot generate")
    }
    return built
}

func finish(_ session: LanguageModelSession, reason: String, maxTokens: Int) {
    var event: [String: Any] = ["type": "done", "finish_reason": reason]
    if #available(macOS 27.0, *) {
        let usage = session.usage
        let completion = usage.output.totalTokenCount
        event["usage"] = [
            "prompt_tokens": usage.input.totalTokenCount,
            "cached_prompt_tokens": usage.input.cachedTokenCount,
            "completion_tokens": completion,
        ]
        if reason == "stop", maxTokens > 0, completion >= maxTokens {
            event["finish_reason"] = "length"
        }
    }
    emit(event)
}

@main
struct DialAppleFM {
    static func main() async {
        switch CommandLine.arguments.dropFirst().first {
        case "availability":
            availability()
        case "chat":
            await chat()
        case "version":
            print(protocolVersion)
        default:
            FileHandle.standardError.write(
                Data("usage: dial-apple-fm availability|chat|version\n".utf8))
            exit(2)
        }
    }
}
