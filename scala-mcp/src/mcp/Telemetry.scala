package mcp

// Tracing for tool calls: a span per `tools/call`, exported over OTLP when
// OTEL_EXPORTER_OTLP_ENDPOINT is set (the standard OTEL_* variables configure
// the rest, via the SDK's autoconfigure). Unset — the default on the droplet —
// everything is a no-op and nothing is exported.

import io.opentelemetry.api
import io.opentelemetry.sdk.autoconfigure.AutoConfiguredOpenTelemetrySdk
import zio.*
import zio.telemetry.opentelemetry.OpenTelemetry
import zio.telemetry.opentelemetry.tracing.Tracing

object Telemetry:
  private val sdk: TaskLayer[api.OpenTelemetry] =
    if Env.get("OTEL_EXPORTER_OTLP_ENDPOINT").exists(_.nonEmpty) then
      OpenTelemetry.custom(
        ZIO.fromAutoCloseable(
          ZIO.attempt(
            AutoConfiguredOpenTelemetrySdk
              .builder()
              .addPropertiesSupplier(() => java.util.Map.of("otel.service.name", "mcp-tools"))
              .build()
              .getOpenTelemetrySdk
          )
        )
      )
    else OpenTelemetry.noop

  val layer: TaskLayer[Tracing] = (sdk ++ OpenTelemetry.contextZIO) >>> OpenTelemetry.tracing("mcp-tools")

  // For tests and CLIs: spans go nowhere.
  val noop: ULayer[Tracing] = (OpenTelemetry.noop ++ OpenTelemetry.contextZIO) >>> OpenTelemetry.tracing("mcp-tools")
