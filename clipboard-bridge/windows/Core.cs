using System;
using System.IO;
using System.Text;
using System.Collections.Generic;
using System.Web.Script.Serialization;
namespace MoonlightClipboard {
public interface IClipboard { Contents Read(); void Write(string text); }
// Optional extension: existing text-only IClipboard implementations need not change.
public interface IImageClipboard { byte[] ReadPng(); void WritePng(byte[] png); }
public sealed class Contents { public bool has_text; public string text; public Contents(bool has,string value){has_text=has;text=value;} }
public sealed class Reply { public int Status; public string Body; public Reply(int status,string body){Status=status;Body=body;} }
public static class BusyRetry { public static T Run<T>(Func<T> action,Action<int> delay){for(int attempt=0;;attempt++){try{return action();}catch(System.Runtime.InteropServices.ExternalException){if(attempt>=4)throw;delay(40);}}} }
// Kept in Core.cs so the existing clean-checkout compiler source lists still work.
public sealed class PngRejectedException : Exception {
 public readonly int Status;
 public PngRejectedException(int status){Status=status;}
}
public static class PngCodec {
 public const int MaxDimension=4096, MaxPixels=16*1024*1024, MaxChunks=16384;
 private static readonly byte[] Signature={137,80,78,71,13,10,26,10};
 private static void Invalid(){throw new PngRejectedException(400);}
 private static void Large(){throw new PngRejectedException(413);}
 private static uint U32(byte[] b,int p){return ((uint)b[p]<<24)|((uint)b[p+1]<<16)|((uint)b[p+2]<<8)|b[p+3];}
 private static uint Crc(byte[] b,int p,int n){uint c=0xffffffff;for(int i=p;i<p+n;i++){c^=b[i];for(int k=0;k<8;k++)c=(c>>1)^((c&1)!=0?0xedb88320u:0);}return c^0xffffffff;}
 public static byte[] FromBase64(string text){
  if(text==null||text.Length==0)Invalid();
  if(text.Length>Core.MaxImageBase64)Large();
  if(text.Length%4!=0)Invalid();
  int padding=text.EndsWith("==",StringComparison.Ordinal)?2:text.EndsWith("=",StringComparison.Ordinal)?1:0;
  for(int i=0;i<text.Length-padding;i++){char c=text[i];if(!((c>='A'&&c<='Z')||(c>='a'&&c<='z')||(c>='0'&&c<='9')||c=='+'||c=='/'))Invalid();}
  if((long)text.Length/4*3-padding>Core.MaxImageBytes)Large();
  byte[] bytes;try{bytes=Convert.FromBase64String(text);}catch(FormatException){throw new PngRejectedException(400);}
  if(Convert.ToBase64String(bytes)!=text)Invalid();
  return bytes;
 }
 public static void Validate(byte[] png){
  if(png==null)Invalid();if(png.Length>Core.MaxImageBytes)Large();if(png.Length<45)Invalid();
  for(int i=0;i<Signature.Length;i++)if(png[i]!=Signature[i])Invalid();
  if(U32(png,8)!=13||Encoding.ASCII.GetString(png,12,4)!="IHDR")Invalid();
  uint width=U32(png,16),height=U32(png,20);int depth=png[24],color=png[25];
  bool validDepth=color==0?(depth==1||depth==2||depth==4||depth==8||depth==16):color==3?(depth==1||depth==2||depth==4||depth==8):(color==2||color==4||color==6)&&(depth==8||depth==16);
  if(width==0||height==0||!validDepth||png[26]!=0||png[27]!=0||png[28]>1)Invalid();
  if(width>MaxDimension||height>MaxDimension||(long)width*height>MaxPixels)Large();
  int offset=8,chunks=0;bool idat=false,afterIdat=false,palette=false,transparency=false,ended=false;
  using(var compressed=new MemoryStream()){
   while(offset<png.Length){
    if(png.Length-offset<12)Invalid();uint size=U32(png,offset);long end=(long)offset+12+size;if(end>png.Length)Invalid();int n=(int)size,data=offset+8;
    for(int i=offset+4;i<offset+8;i++){byte c=png[i];if(!((c>=65&&c<=90)||(c>=97&&c<=122)))Invalid();}
    if((png[offset+6]&32)!=0)Invalid(); // Reserved bit in the chunk name.
    if(Crc(png,offset+4,n+4)!=U32(png,data+n))Invalid();if(++chunks>MaxChunks)Invalid();
    string kind=Encoding.ASCII.GetString(png,offset+4,4);
    if(kind=="IHDR"){if(offset!=8)Invalid();}
    else if(kind=="PLTE"){if(palette||idat||n==0||n%3!=0||n>768||color==0||color==4||(color==3&&n/3>(1<<depth)))Invalid();palette=true;}
    else if(kind=="tRNS"){if(transparency||idat||color==4||color==6||(color==0&&n!=2)||(color==2&&n!=6)||(color==3&&(!palette||n==0||n>256)))Invalid();transparency=true;}
    else if(kind=="IDAT"){if(afterIdat||(color==3&&!palette))Invalid();idat=true;compressed.Write(png,data,n);}
    else if(kind=="IEND"){if(!idat||n!=0||end!=png.Length)Invalid();ended=true;break;}
    else{if((png[offset+4]&32)==0||kind=="acTL"||kind=="fcTL"||kind=="fdAT")Invalid();}
    if(idat&&kind!="IDAT")afterIdat=true;offset=(int)end;
   }
   if(!ended)Invalid();
   int channels=color==0||color==3?1:color==2?3:color==4?2:4;
   ValidateInflation(compressed.ToArray(),(int)width,(int)height,channels*depth,png[28]!=0);
  }
 }
 private static void ValidateInflation(byte[] z,int width,int height,int bits,bool interlaced){
  if(z.Length<6||(z[0]&15)!=8||(z[0]>>4)>7||(((int)z[0]<<8)|z[1])%31!=0||(z[1]&32)!=0)Invalid();
  int[] sx=interlaced?new[]{0,4,0,2,0,1,0}:new[]{0};
  int[] sy=interlaced?new[]{0,0,4,0,2,0,1}:new[]{0};
  int[] dx=interlaced?new[]{8,8,4,4,2,2,1}:new[]{1};
  int[] dy=interlaced?new[]{8,8,8,4,4,2,2}:new[]{1};
  uint a=1,b=0;
  try{
   using(var input=new MemoryStream(z,2,z.Length-6,false))
   using(var inflate=new System.IO.Compression.DeflateStream(input,System.IO.Compression.CompressionMode.Decompress)){
    byte[] row=new byte[(width*bits+7)/8+1];
    for(int pass=0;pass<sx.Length;pass++){
     int w=width<=sx[pass]?0:(width-sx[pass]+dx[pass]-1)/dx[pass],h=height<=sy[pass]?0:(height-sy[pass]+dy[pass]-1)/dy[pass];
     if(w==0||h==0)continue;int length=(w*bits+7)/8+1;
     for(int y=0;y<h;y++){
      int read=0;while(read<length){int n=inflate.Read(row,read,length-read);if(n==0)Invalid();read+=n;}
      if(row[0]>4)Invalid();for(int i=0;i<length;i++){a=(a+row[i])%65521;b=(b+a)%65521;}
     }
    }
    if(inflate.ReadByte()!=-1)Invalid(); // Bound expansion to exactly the IHDR scanlines.
   }
  }catch(InvalidDataException){throw new PngRejectedException(400);}catch(IOException){throw new PngRejectedException(400);}
  if((b<<16|a)!=U32(z,z.Length-4))Invalid();
 }
 private static byte[] PixelChunks(byte[] png){
  // Never feed compressed ancillary metadata (iCCP/zTXt/iTXt, etc.) to GDI+.
  // Only pixel-bearing chunks survive; dimensions and inflated scanlines have
  // already been bounded. This also avoids retaining clipboard metadata.
  using(var output=new MemoryStream()){
   output.Write(Signature,0,Signature.Length);int offset=8;
   while(offset<png.Length){int n=(int)U32(png,offset);string kind=Encoding.ASCII.GetString(png,offset+4,4);
    if(kind=="IHDR"||kind=="PLTE"||kind=="tRNS"||kind=="IDAT"||kind=="IEND")output.Write(png,offset,n+12);
    offset+=n+12;
   }return output.ToArray();
  }
 }
 public static System.Drawing.Bitmap Decode(byte[] png){
  Validate(png);
  try{
   using(var input=new MemoryStream(PixelChunks(png),false))
   using(var image=System.Drawing.Image.FromStream(input,false,true)){
    if(image.RawFormat.Guid!=System.Drawing.Imaging.ImageFormat.Png.Guid||image.Width!=(int)U32(png,16)||image.Height!=(int)U32(png,20))Invalid();
    var clone=new System.Drawing.Bitmap(image.Width,image.Height,System.Drawing.Imaging.PixelFormat.Format32bppArgb);
    try{using(var graphics=System.Drawing.Graphics.FromImage(clone)){graphics.CompositingMode=System.Drawing.Drawing2D.CompositingMode.SourceCopy;graphics.DrawImage(image,new System.Drawing.Rectangle(0,0,clone.Width,clone.Height));}return clone;}
    catch{clone.Dispose();throw;}
   }
  }catch(ArgumentException){throw new PngRejectedException(400);}catch(System.Runtime.InteropServices.ExternalException){throw new PngRejectedException(400);}
 }
}

// Native delegates transfer ownership of a read image to this adapter; a write
// delegate consumes the image synchronously and must not retain it after return.
public sealed class PngClipboardAdapter : IImageClipboard {
 private readonly Func<System.Drawing.Image> read;
 private readonly Action<System.Drawing.Image> write;
 private readonly Action<int> delay;
 public static PngClipboardAdapter CreateNative(){
  // Constructing this adapter and capability negotiation perform no OS calls.
  return new PngClipboardAdapter(delegate {return System.Windows.Forms.Clipboard.GetImage();},delegate(System.Drawing.Image image){
   var data=new System.Windows.Forms.DataObject();data.SetData(System.Windows.Forms.DataFormats.Bitmap,false,image);
   System.Windows.Forms.Clipboard.SetDataObject(data,true,0,0);
  },System.Threading.Thread.Sleep);
 }
 public PngClipboardAdapter(Func<System.Drawing.Image> reader,Action<System.Drawing.Image> writer,Action<int> sleep){read=reader;write=writer;delay=sleep;}
 private sealed class BoundedPngStream : MemoryStream {
  public bool LimitExceeded;
  private void Check(long position){if(position>Core.MaxImageBytes){LimitExceeded=true;throw new PngRejectedException(413);}}
  public override void Write(byte[] buffer,int offset,int count){Check(Position+count);base.Write(buffer,offset,count);}
  public override void WriteByte(byte value){Check(Position+1);base.WriteByte(value);}
  public override void SetLength(long value){Check(value);base.SetLength(value);}
  public override long Seek(long offset,SeekOrigin origin){long target=origin==SeekOrigin.Begin?offset:origin==SeekOrigin.Current?Position+offset:Length+offset;Check(target);return base.Seek(offset,origin);}
 }
 public byte[] ReadPng(){
  RequireSta();return BusyRetry.Run<byte[]>(delegate {
   using(var source=read()){
    if(source==null)return null;
    if(source.Width>PngCodec.MaxDimension||source.Height>PngCodec.MaxDimension||(long)source.Width*source.Height>PngCodec.MaxPixels)throw new PngRejectedException(413);
    using(var clone=new System.Drawing.Bitmap(source.Width,source.Height,System.Drawing.Imaging.PixelFormat.Format32bppArgb))
    using(var stream=new BoundedPngStream()){
     using(var graphics=System.Drawing.Graphics.FromImage(clone)){graphics.CompositingMode=System.Drawing.Drawing2D.CompositingMode.SourceCopy;graphics.DrawImage(source,new System.Drawing.Rectangle(0,0,clone.Width,clone.Height));}
     try{clone.Save(stream,System.Drawing.Imaging.ImageFormat.Png);}catch(Exception){if(stream.LimitExceeded)throw new PngRejectedException(413);throw;}
     if(stream.LimitExceeded)throw new PngRejectedException(413);
     byte[] png=stream.ToArray();PngCodec.Validate(png);return png;
    }
   }
  },delay);
 }
 private static void RequireSta(){if(System.Threading.Thread.CurrentThread.GetApartmentState()!=System.Threading.ApartmentState.STA)throw new InvalidOperationException("Clipboard requires STA");}
 public void WritePng(byte[] png){
  RequireSta();using(var image=PngCodec.Decode(png)){
   BusyRetry.Run<int>(delegate {write(image);return 0;},delay);
  }
 }
}
public sealed class Core {
 private static void CheckDeadline(Stream stream,System.Diagnostics.Stopwatch clock,int deadlineMs){
 int remaining=deadlineMs-(int)clock.ElapsedMilliseconds;if(remaining<=0)throw new TimeoutException();if(stream.CanTimeout)stream.ReadTimeout=remaining;
 }
 public Reply Parse(Stream stream,string peer,int deadlineMs){
 if(peer!=Peer)return new Reply(403,"");
 var clock=System.Diagnostics.Stopwatch.StartNew();
 try {
 var header=new List<byte>();bool complete=false;
 while(header.Count<8192){CheckDeadline(stream,clock,deadlineMs);int value=stream.ReadByte();if(value<0)return new Reply(400,"");if(value>126||(value<32&&value!=13&&value!=10))return new Reply(400,"");header.Add((byte)value);int n=header.Count;if(n>=4&&header[n-4]==13&&header[n-3]==10&&header[n-2]==13&&header[n-1]==10){complete=true;break;}}
 if(!complete)return new Reply(431,"");
 string[] lines=Encoding.ASCII.GetString(header.ToArray()).Split(new[]{"\r\n"},StringSplitOptions.None);
 string[] request=lines[0].Split(' ');if(request.Length!=3||(request[2]!="HTTP/1.1"&&request[2]!="HTTP/1.0"))return new Reply(400,"");
 var headers=new Dictionary<string,string>(StringComparer.OrdinalIgnoreCase);
 for(int i=1;i<lines.Length;i++){
 if(lines[i].Length==0)break;int colon=lines[i].IndexOf(':');if(colon<=0)return new Reply(400,"");string name=lines[i].Substring(0,colon);
 foreach(char c in name)if(!char.IsLetterOrDigit(c)&&c!='-')return new Reply(400,"");
 if(headers.ContainsKey(name))return new Reply(400,"");string v=lines[i].Substring(colon+1).Trim();if(v.IndexOf('\r')>=0||v.IndexOf('\n')>=0)return new Reply(400,"");headers.Add(name,v);
 }
 if(headers.ContainsKey("Transfer-Encoding"))return new Reply(400,"");
 string auth;if(!headers.TryGetValue("Authorization",out auth)||!ConstantEquals(auth,"Bearer "+Token))return new Reply(401,"");
 int maxBody=request[0]=="POST"&&request[1]=="/clipboard/image"?MaxImageBody:MaxBody;
 int length=0;string lengthValue;if(headers.TryGetValue("Content-Length",out lengthValue)){
 if(lengthValue.Length==0)return new Reply(400,"");foreach(char c in lengthValue)if(c<'0'||c>'9')return new Reply(400,"");
 if(!int.TryParse(lengthValue,out length)||length>maxBody)return new Reply(413,"");
 }
 if(request[0]=="GET"&&length!=0)return new Reply(400,"");
 byte[] body=new byte[length];int read=0;while(read<length){CheckDeadline(stream,clock,deadlineMs);int n=stream.Read(body,read,length-read);if(n==0)return new Reply(400,"");read+=n;}
 CheckDeadline(stream,clock,deadlineMs);return Handle(peer,request[0],request[1],headers,body);
 }catch(TimeoutException){return new Reply(408,"");}catch(IOException){return new Reply(408,"");}
 }
 public static byte[] Encode(Reply reply){
 byte[] body=Encoding.UTF8.GetBytes(reply.Body);byte[] head=Encoding.ASCII.GetBytes("HTTP/1.1 "+reply.Status+" Reply\r\nContent-Length: "+body.Length+"\r\nContent-Type: application/json; charset=utf-8\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n");
 byte[] frame=new byte[head.Length+body.Length];Buffer.BlockCopy(head,0,frame,0,head.Length);Buffer.BlockCopy(body,0,frame,head.Length,body.Length);return frame;
 }
 public const int MaxText=65536, MaxBody=524288;
 public const int MaxImageBytes=4*1024*1024, MaxImageBase64=((MaxImageBytes+2)/3)*4, MaxImageBody=MaxImageBase64+1024;
 public readonly IClipboard Backend; public readonly string Token; public readonly string Peer;
 public Core(IClipboard backend,string token,string peer){Backend=backend;Token=token;Peer=peer;}
 public static bool ConstantEquals(string a,string b){int difference=a.Length^b.Length;for(int i=0;i<b.Length;i++)difference|=(i<a.Length?a[i]:0)^b[i];return difference==0;}
 public Reply Handle(string peer,string method,string path,Dictionary<string,string> headers,byte[] body){
 try{return HandleInner(peer,method,path,headers,body);}catch(PngRejectedException e){return new Reply(e.Status,"");}catch(Exception){return new Reply(503,"");}
 }
 private Reply HandleInner(string peer,string method,string path,Dictionary<string,string> headers,byte[] body){if(peer!=Peer)return new Reply(403,"");
 string auth; if(!headers.TryGetValue("Authorization",out auth)||!ConstantEquals(auth,"Bearer "+Token))return new Reply(401,"");
 var image=Backend as IImageClipboard;
 if(path=="/capabilities")return method=="GET"?new Reply(200,image==null?"{\"formats\":[\"text/plain\"]}":"{\"formats\":[\"text/plain\",\"image/png\"],\"max_image_bytes\":4194304,\"max_image_dimension\":4096,\"max_image_pixels\":16777216}"):new Reply(405,"");
 if(path=="/clipboard/image"){
  if(image==null)return new Reply(404,"");
  if(method!="GET"&&method!="POST")return new Reply(405,"");
  if(method=="GET"){
   byte[] png=image.ReadPng();if(png!=null){try{PngCodec.Validate(png);}catch(PngRejectedException e){return new Reply(e.Status==413?413:503,"");}}
   var output=new JavaScriptSerializer();output.MaxJsonLength=MaxImageBody;
   return new Reply(200,output.Serialize(new {has_image=png!=null,png_base64=png==null?"":Convert.ToBase64String(png)}));
  }
  if(body.Length>MaxImageBody)return new Reply(413,"");
  string contentType;if(!headers.TryGetValue("Content-Type",out contentType)||contentType.Split(';')[0].Trim().ToLowerInvariant()!="application/json")return new Reply(415,"");
  byte[] decoded;
  try{
   var serializer=new JavaScriptSerializer();serializer.MaxJsonLength=MaxImageBody;serializer.RecursionLimit=8;
   var value=serializer.DeserializeObject(new UTF8Encoding(false,true).GetString(body)) as Dictionary<string,object>;
   object raw;if(value==null||value.Count!=1||!value.TryGetValue("png_base64",out raw)||!(raw is string))return new Reply(400,"");
   decoded=PngCodec.FromBase64((string)raw);PngCodec.Validate(decoded);
  }catch(PngRejectedException e){return new Reply(e.Status,"");}catch(ArgumentException){return new Reply(400,"");}catch(InvalidOperationException){return new Reply(400,"");}
  image.WritePng(decoded);return new Reply(204,"");
 }
 if(path!="/clipboard")return new Reply(404,"");
 if(method!="GET"&&method!="POST")return new Reply(405,"");
 if(method=="POST") {
 if(body.Length>MaxBody)return new Reply(413,"");
 string type;if(!headers.TryGetValue("Content-Type",out type)||type.Split(';')[0].Trim().ToLowerInvariant()!="application/json")return new Reply(415,"");
 string text;
 try {
 var serializer=new JavaScriptSerializer();serializer.MaxJsonLength=MaxBody;serializer.RecursionLimit=8;
 var value=serializer.DeserializeObject(new UTF8Encoding(false,true).GetString(body)) as Dictionary<string,object>;
 object raw;if(value==null||!value.TryGetValue("text",out raw)||!(raw is string))return new Reply(400,"");
 text=(string)raw;if(text.IndexOf('\0')>=0||new UTF8Encoding(false,true).GetByteCount(text)>MaxText)return new Reply(400,"");
 }catch(ArgumentException){return new Reply(400,"");}catch(InvalidOperationException){return new Reply(400,"");}
 Backend.Write(text);return new Reply(204,"");
 }
 var result=Backend.Read();
 if(result.text==null||result.text.IndexOf('\0')>=0||(!result.has_text&&result.text.Length!=0))return new Reply(503,"");
 if(new UTF8Encoding(false,true).GetByteCount(result.text)>MaxText)return new Reply(413,"");
 return new Reply(200,new JavaScriptSerializer().Serialize(result));}
}
}
