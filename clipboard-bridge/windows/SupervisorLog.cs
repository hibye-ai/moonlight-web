using System;
using System.Globalization;
using System.IO;

namespace MoonlightClipboard {
/// <summary>
/// Bounded, content-free supervisor logging.
///
/// Only status strings are ever written; request text, clipboard contents,
/// tokens and peer addresses must not be passed here. Rotation cannot silently
/// stop logging: a failed rotation is reported and appending continues, and the
/// number of retained generations is explicit.
/// </summary>
public static class SupervisorLog {
 public const long MaxBytes = 65536;
 public const int RetainedGenerations = 2;

 public static string PathFor(string root, int generation) {
  string basePath = Path.Combine(root, "supervisor.log");
  return generation == 0 ? basePath : basePath + "." + generation.ToString(CultureInfo.InvariantCulture);
 }

 /// <summary>
 /// Rotate when the active log is over the limit, keeping a bounded number of
 /// older generations. An existing rotation target is replaced rather than
 /// making File.Move throw, which used to skip the append entirely on every
 /// rotation after the first.
 /// </summary>
 public static bool RotateIfNeeded(string root) {
  string active = PathFor(root, 0);
  FileInfo info = new FileInfo(active);
  if (!info.Exists || info.Length <= MaxBytes) return false;

  // Drop the oldest generation, then shift the rest down one slot.
  string oldest = PathFor(root, RetainedGenerations);
  if (File.Exists(oldest)) File.Delete(oldest);
  for (int generation = RetainedGenerations - 1; generation >= 1; generation--) {
   string from = PathFor(root, generation);
   if (File.Exists(from)) File.Move(from, PathFor(root, generation + 1));
  }
  File.Move(active, PathFor(root, 1));
  return true;
 }

 /// <summary>
 /// Append one status line. Returns true when the line was persisted.
 /// Rotation and append failures are distinguished so a caller (or a human
 /// reading the last log) can tell "log too big" from "log not writable".
 /// </summary>
 public static bool Append(string root, string value) {
  string active = PathFor(root, 0);
  try {
   RotateIfNeeded(root);
  } catch (Exception) {
   // Rotation failed (locked/permission). Keep logging to the active file
   // instead of dropping recovery history.
   try { File.AppendAllText(active, Line("rotation-failed " + value)); } catch (Exception) { }
   return false;
  }
  try {
   File.AppendAllText(active, Line(value));
   return true;
  } catch (Exception) {
   return false;
  }
 }

 static string Line(string value) {
  return DateTime.UtcNow.ToString("o", CultureInfo.InvariantCulture) + " " + value + Environment.NewLine;
 }
}
}
