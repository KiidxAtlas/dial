---
name: art-to-ui
description: Generate artwork with image_gen, inspect it, build or update the UI around it, then confirm the rendered page with a browser screenshot. Covers remixing the art and updating the UI to match.
---

# Art into UI

## Outcome
A generated asset (hero, title art, mascot, background) that the UI really uses, confirmed by a
fresh screenshot of the rendered page, not only by reading the source.

## Scope
Use when the user asks for artwork that a page, screen or component should be built on or around.
Plain image requests without UI work need only `image_gen`. Every `image_gen` call needs approval;
never treat an earlier approval as covering a new generation or edit.

## Workflow
1. Establish the target: the page or component file, the framework's static asset directory (for
   example `public/` or `static/`), the size and aspect the layout needs, and the style. Read the
   target file and how it already loads assets before generating anything.
2. Generate with `image_gen` into a new file under the asset directory, with an explicit `output`
   whose extension matches the backend's format. Existing files are never overwritten, so pick a
   new name instead of reusing one.
3. Inspect the result. With a vision-capable model the image is attached: check subject, framing,
   contrast against the planned text and the space left for overlays. If it misses, revise with an
   `image_gen` edit (`image` set to the previous file) into another new file rather than
   regenerating blindly.
4. Build the UI around the asset: reference it by the path the framework serves, size it for the
   layout, and take colours or spacing from the art only where they keep text readable. Keep edits
   in the target files the user named.
5. Check the rendered result: start or reuse the dev server as a background job, open the page with
   the `browser` tool, and take a `screenshot` (add `full_page` for long pages). Compare it with
   the art and fix broken paths, cropping, contrast or layout shifts, then screenshot again.
6. Remix on request: edit the current art into a new file, point the UI at the new path, remove
   references to the old file only when the user wants it replaced, and repeat the screenshot check.

## Verification
The final screenshot must come after the last UI or asset change. When the browser tool, a dev
server or a vision model is unavailable, say which check could not run instead of claiming the page
renders correctly.

## Report
Name the generated files, the UI files changed, the backend used, and the screenshot evidence.
State any unverified aspect, such as appearance on other viewport sizes.
