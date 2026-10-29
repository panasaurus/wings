// Panasaurus Wings — OpenAPI 3.0 documentation endpoint
//
// Serves a hand-curated OpenAPI 3.0.3 document at `GET /openapi.json` that
// describes the public Wings HTTP API. This mirrors the wings-main
// `GET /openapi.json` endpoint without pulling in the full utoipa derive
// macro stack (which would require annotating every handler). The spec is
// built once at startup from a static template and is cheap to serve.
//
// Clients (Swagger UI, Postman, panel integrations) can fetch this to
// discover available endpoints, request/response shapes, and auth model.

use axum::Json;
use serde_json::{Value, json};

/// Build the OpenAPI 3.0.3 document for the Panasaurus Wings HTTP API.
///
/// The document describes:
/// - the bearer-token auth scheme (`Authorization: Bearer <node-token>`)
/// - all routes registered in `api::router`
/// - the `/download/*` and `/upload/*` token-authenticated routes
/// - the new v0.2.x endpoints (`/api/stats`, `/api/system/logs`,
///   `/api/system/upgrade`, `/api/servers/{id}/files/search`,
///   `/api/servers/{id}/files/revisions`, `/api/servers/{id}/script`)
pub fn document() -> Value {
    let version = env!("CARGO_PKG_VERSION");
    json!({
        "openapi": "3.0.3",
        "info": {
            "title": "Panasaurus Wings API",
            "description": "HTTP API for the Panasaurus Wings server control daemon. All routes under `/api/*` require a `Bearer` token matching the node's `token` configuration value, except where noted. Routes under `/download/*` and `/upload/*` use short-lived JWTs issued by the panel.",
            "version": version,
            "license": { "name": "MIT" },
            "contact": { "name": "Panasaurus", "url": "https://panasaurus.com" }
        },
        "servers": [
            { "url": "/", "description": "This node" }
        ],
        "components": {
            "securitySchemes": {
                "BearerToken": {
                    "type": "http",
                    "scheme": "bearer",
                    "bearerFormat": "plain",
                    "description": "The node's `config.token` value, sent as `Authorization: Bearer <token>`."
                }
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "properties": {
                        "error": { "type": "string" }
                    },
                    "required": ["error"]
                },
                "Server": {
                    "type": "object",
                    "description": "A server assigned to this node."
                },
                "PowerAction": {
                    "type": "object",
                    "properties": {
                        "action": { "type": "string", "enum": ["start", "stop", "restart", "kill"] }
                    },
                    "required": ["action"]
                },
                "FileUpload": {
                    "type": "object",
                    "description": "Multipart file upload, authenticated by a panel-issued JWT."
                },
                "Stats": {
                    "type": "object",
                    "description": "Node-wide resource usage. Aggregated across all servers.",
                    "properties": {
                        "booted_at": { "type": "integer", "description": "Unix timestamp when wings started." },
                        "servers": { "type": "integer" },
                        "disk": {
                            "type": "object",
                            "properties": {
                                "total": { "type": "integer" },
                                "used": { "type": "integer" }
                            }
                        },
                        "memory": {
                            "type": "object",
                            "properties": {
                                "total": { "type": "integer" },
                                "used": { "type": "integer" }
                            }
                        }
                    }
                }
            }
        },
        "security": [{ "BearerToken": [] }],
        "paths": {
            "/api/system": {
                "get": {
                    "summary": "Node system information",
                    "description": "Returns Docker version, OS, kernel, CPU count, and disk space. Supports `?v=2` for the extended payload.",
                    "tags": ["system"],
                    "parameters": [
                        { "name": "v", "in": "query", "required": false, "schema": { "type": "string" }, "description": "Pass `v=2` for the extended payload." }
                    ],
                    "responses": {
                        "200": { "description": "System info", "content": { "application/json": { "schema": { "type": "object" } } } }
                    }
                }
            },
            "/api/stats": {
                "get": {
                    "summary": "Node-wide resource usage",
                    "description": "Aggregated CPU, memory, and disk usage across all servers on this node. New in v0.2.1.",
                    "tags": ["system"],
                    "responses": {
                        "200": { "description": "Node stats", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Stats" } } } }
                    }
                }
            },
            "/api/system/logs": {
                "get": {
                    "summary": "List wings log files",
                    "description": "Returns a list of wings log files available for reading. New in v0.2.1.",
                    "tags": ["system"],
                    "responses": {
                        "200": { "description": "Log file list", "content": { "application/json": { "schema": { "type": "object" } } } }
                    }
                }
            },
            "/api/system/logs/{file}": {
                "get": {
                    "summary": "Read a wings log file",
                    "description": "Returns the contents of a wings log file. New in v0.2.1.",
                    "tags": ["system"],
                    "parameters": [
                        { "name": "file", "in": "path", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": {
                        "200": { "description": "Log file contents", "content": { "application/json": { "schema": { "type": "object" } } } },
                        "404": { "description": "Log file not found", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/system/upgrade": {
                "post": {
                    "summary": "Remotely upgrade the wings binary",
                    "description": "Downloads a new wings binary from the provided URL and atomically swaps it in. Requires a node restart to take effect. New in v0.2.1.",
                    "tags": ["system"],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "properties": {
                                        "url": { "type": "string", "format": "uri" },
                                        "checksum": { "type": "string", "description": "Optional sha256 hex digest of the binary." }
                                    },
                                    "required": ["url"]
                                }
                            }
                        }
                    },
                    "responses": {
                        "202": { "description": "Upgrade scheduled" },
                        "400": { "description": "Invalid request", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/api/servers": {
                "get": {
                    "summary": "List servers on this node",
                    "tags": ["servers"],
                    "responses": { "200": { "description": "Server list", "content": { "application/json": { "schema": { "type": "array", "items": { "$ref": "#/components/schemas/Server" } } } } } }
                },
                "post": {
                    "summary": "Create / install a server",
                    "tags": ["servers"],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object", "properties": { "uuid": { "type": "string" }, "start_on_completion": { "type": "boolean" } }, "required": ["uuid"] } } } },
                    "responses": { "202": { "description": "Installation scheduled" } }
                }
            },
            "/api/servers/{id}": {
                "get": {
                    "summary": "Get server details",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Server details" }, "404": { "description": "Server not found" } }
                },
                "delete": {
                    "summary": "Delete a server",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/servers/{id}/power": {
                "post": {
                    "summary": "Send a power action",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "$ref": "#/components/schemas/PowerAction" } } } },
                    "responses": { "202": { "description": "Action accepted" } }
                }
            },
            "/api/servers/{id}/commands": {
                "post": {
                    "summary": "Send console commands",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": { "required": true, "content": { "application/json": { "schema": { "type": "object", "properties": { "commands": { "type": "array", "items": { "type": "string" } } }, "required": ["commands"] } } } },
                    "responses": { "204": { "description": "Commands sent" } }
                }
            },
            "/api/servers/{id}/logs": {
                "get": {
                    "summary": "Get recent console logs",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "200": { "description": "Log lines" } }
                }
            },
            "/api/servers/{id}/files/list-directory": {
                "get": {
                    "summary": "List files in a directory",
                    "tags": ["files"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "directory", "in": "query", "required": false, "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "File listing" } }
                }
            },
            "/api/servers/{id}/files/contents": {
                "get": {
                    "summary": "Get file contents",
                    "tags": ["files"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "file", "in": "query", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "File contents" } }
                }
            },
            "/api/servers/{id}/files/search": {
                "post": {
                    "summary": "Search files by name or content",
                    "description": "Multi-threaded file search across the server's filesystem. Returns matching paths with optional content previews. New in v0.2.1.",
                    "tags": ["files"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "properties": {
                                        "pattern": { "type": "string", "description": "Regex or glob pattern." },
                                        "mode": { "type": "string", "enum": ["filename", "content"], "default": "filename" },
                                        "directory": { "type": "string", "default": "/" },
                                        "max_matches": { "type": "integer", "default": 100 }
                                    },
                                    "required": ["pattern"]
                                }
                            }
                        }
                    },
                    "responses": { "200": { "description": "Search results" } }
                }
            },
            "/api/servers/{id}/files/revisions": {
                "get": {
                    "summary": "List file revision history",
                    "description": "Returns prior revisions of a file tracked by the file_history subsystem. New in v0.2.1.",
                    "tags": ["files"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "file", "in": "query", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "Revision list" } }
                }
            },
            "/api/servers/{id}/files/revisions/{revision}": {
                "get": {
                    "summary": "Get a specific file revision",
                    "description": "Returns the contents of a tracked file revision. New in v0.2.1.",
                    "tags": ["files"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "revision", "in": "path", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": { "200": { "description": "Revision contents" }, "404": { "description": "Revision not found" } }
                }
            },
            "/api/servers/{id}/backup": {
                "post": {
                    "summary": "Create a backup",
                    "tags": ["backups"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "202": { "description": "Backup scheduled" } }
                }
            },
            "/api/servers/{id}/backup/{backup}": {
                "delete": {
                    "summary": "Delete a backup",
                    "tags": ["backups"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "backup", "in": "path", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": { "204": { "description": "Deleted" } }
                }
            },
            "/api/servers/{id}/backup/{backup}/restore": {
                "post": {
                    "summary": "Restore a backup",
                    "tags": ["backups"],
                    "parameters": [
                        { "name": "id", "in": "path", "required": true, "schema": { "type": "string" } },
                        { "name": "backup", "in": "path", "required": true, "schema": { "type": "string" } }
                    ],
                    "responses": { "202": { "description": "Restore scheduled" } }
                }
            },
            "/api/servers/{id}/ws": {
                "get": {
                    "summary": "WebSocket console stream",
                    "description": "Upgrade to a WebSocket to receive console output and send commands. Authenticated by a panel-issued JWT in the query string.",
                    "tags": ["servers"],
                    "parameters": [{ "name": "id", "in": "path", "required": true, "schema": { "type": "string" } }],
                    "responses": { "101": { "description": "Switching Protocols" } }
                }
            }
        }
    })
}

/// Axum handler that serves the OpenAPI document as JSON.
pub async fn serve() -> Json<Value> {
    Json(document())
}
