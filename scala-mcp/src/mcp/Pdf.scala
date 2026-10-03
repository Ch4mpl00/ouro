package mcp

// PDF → plain text, for the bills download_gmail_attachment saves. Pages are
// joined with one blank line so the agent can still see the page breaks.
//
// Sections:
//   1. extract — file → { text, numPages }
//   2. tools   — `pdf` toolset: read_pdf

import org.apache.pdfbox.Loader
import org.apache.pdfbox.text.PDFTextStripper
import sttp.tapir.Schema
import sttp.tapir.Schema.annotations.description
import zio.*
import zio.json.*

import java.nio.file.Files
import java.nio.file.Path

// ── 1. extract ───────────────────────────────────────────────────────────────

final case class PdfText(text: String, numPages: Int) derives JsonEncoder

object Pdf:
  def extract(bytes: Array[Byte]): PdfText =
    val doc = Loader.loadPDF(bytes)
    try
      val stripper = PDFTextStripper()
      val pages = (1 to doc.getNumberOfPages).map { n =>
        stripper.setStartPage(n)
        stripper.setEndPage(n)
        stripper.getText(doc)
      }
      PdfText(pages.mkString("\n\n"), pages.size)
    finally doc.close()

  // Parsing is CPU-bound: off the async pool.
  def read(path: Path): Task[PdfText] =
    ZIO.attemptBlocking(extract(Files.readAllBytes(path)))

// ── 2. tools ─────────────────────────────────────────────────────────────────

object PdfTools:
  import Tools.*

  final case class ReadPdfParams(@description("Absolute path to the PDF file.") filePath: String)
      derives JsonDecoder,
        Schema

  val tools: List[ToolDef] = List(
    tool(
      "read_pdf",
      "Read PDF",
      "Extract plain text from a PDF file at the given absolute path. Returns the full text (pages separated by a " +
        "blank line) and total page count. Use after download_gmail_attachment to inspect the contents of a " +
        "downloaded bill."
    ) { (_, p: ReadPdfParams) => Pdf.read(Path.of(p.filePath)) }
  )
