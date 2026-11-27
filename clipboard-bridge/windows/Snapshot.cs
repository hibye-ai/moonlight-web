using System;
using System.IO;
using System.Text;
using System.Collections.Generic;
using System.Drawing;
using System.Windows.Forms;
namespace MoonlightClipboard {
// Captures only fully materializable values. Unknown formats fail closed before ready.
public sealed class Snapshot {
 public DataObject Data; private Dictionary<string,byte[]> signatures = new Dictionary<string,byte[]>();
 private static object Clone(object value){
 if((value is byte[]&&((byte[])value).Length>32*1024*1024)||(value is string&&((string)value).Length>16*1024*1024)||(value is Bitmap&&((long)((Bitmap)value).Width*((Bitmap)value).Height>8*1024*1024)))throw new InvalidOperationException("Unmaterializable format");
 if(value is string || value is int || value is long || value is bool || value is double)return value;
 if(value is byte[])return ((byte[])value).Clone();
 if(value is string[])return ((string[])value).Clone();
 if(value is Bitmap)return new Bitmap((Bitmap)value);
 var stream=value as Stream;if(stream!=null){if(!stream.CanSeek||stream.Length>32*1024*1024)throw new InvalidOperationException("Unmaterializable format");long position=stream.Position;try{stream.Position=0;var copy=new MemoryStream();stream.CopyTo(copy);copy.Position=0;return copy;}finally{stream.Position=position;}}
 throw new InvalidOperationException("Unmaterializable format");
 }
 private static byte[] Signature(object value){
 using(var bytes=new MemoryStream())using(var writer=new BinaryWriter(bytes,Encoding.UTF8)){
 writer.Write(value.GetType().FullName);
 if(value is string)writer.Write((string)value);
 else if(value is byte[])writer.Write((byte[])value);
 else if(value is string[]){foreach(string s in (string[])value)writer.Write(s);}
 else if(value is Bitmap){var b=(Bitmap)value;writer.Write(b.Width);writer.Write(b.Height);for(int y=0;y<b.Height;y++)for(int x=0;x<b.Width;x++)writer.Write(b.GetPixel(x,y).ToArgb());}
 else if(value is Stream){var s=(Stream)value;long position=s.Position;try{s.Position=0;s.CopyTo(bytes);}finally{s.Position=position;}}
 else writer.Write(Convert.ToString(value,System.Globalization.CultureInfo.InvariantCulture));
 writer.Flush();using(var sha=System.Security.Cryptography.SHA256.Create())return sha.ComputeHash(bytes.ToArray());
 }
 }
 public static Snapshot Capture(IDataObject source){
 var result=new Snapshot();result.Data=new DataObject();if(source==null)return result;
 string[] formats=source.GetFormats(false);if(formats.Length>128)throw new InvalidOperationException("Too many formats");
 foreach(string format in formats){
 object value=Clone(source.GetData(format,false));result.Data.SetData(format,false,value);result.signatures.Add(format,Signature(value));
 }
 return result;
 }
 public bool Matches(IDataObject current){
 try{var copy=Capture(current);if(copy.signatures.Count!=signatures.Count)return false;
 foreach(var pair in signatures){byte[] other;if(!copy.signatures.TryGetValue(pair.Key,out other)||!Convert.ToBase64String(pair.Value).Equals(Convert.ToBase64String(other)))return false;}return true;
 }catch{return false;}
 }
}
}
