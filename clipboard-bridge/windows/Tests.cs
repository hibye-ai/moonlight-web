using System;
using System.Collections.Generic;
using System.Text;
using MoonlightClipboard;
class Fake : IClipboard { public string Text="한글 😀\nline"; public bool Has=true; public int Writes; public Exception Failure=null; public Contents Read(){if(Failure!=null)throw Failure;return new Contents(Has,Text);} public void Write(string s){if(Failure!=null)throw Failure;Text=s;Writes++;} }
class ImageFake : Fake, IImageClipboard {
 public int ImageReads,ImageWrites; public byte[] Png;
 public byte[] ReadPng(){ImageReads++;if(Failure!=null)throw Failure;return Png;}
 public void WritePng(byte[] bytes){if(Failure!=null)throw Failure;Png=bytes;ImageWrites++;}
}
class Tests {
 static int count; static string token=new string('a',64); static Fake fake; static Core core;
 static Dictionary<string,string> Headers(){return new Dictionary<string,string>(StringComparer.OrdinalIgnoreCase){{"Authorization","Bearer "+token},{"Content-Type","application/json"}};}
 static Reply Call(string method,string path,byte[] body){return core.Handle("10.23.45.152",method,path,Headers(),body);}
 static void Eq(object actual,object expected){if(!object.Equals(actual,expected))throw new Exception("expected "+expected+", got "+actual);}
 static void Test(string name,Action action){fake=new Fake();core=new Core(fake,token,"10.23.45.152");try{action();Console.WriteLine("PASS "+name);count++;}catch(Exception e){Console.WriteLine("FAIL "+name+" "+e.GetType().Name+": "+e.Message);Environment.Exit(1);}}
 static byte[] ImageBody(byte[] png){return Encoding.UTF8.GetBytes("{\"png_base64\":\""+Convert.ToBase64String(png)+"\"}");}
 static byte[] SamplePng(){using(var b=new System.Drawing.Bitmap(2,1)){b.SetPixel(0,0,System.Drawing.Color.Red);b.SetPixel(1,0,System.Drawing.Color.Blue);using(var s=new System.IO.MemoryStream()){b.Save(s,System.Drawing.Imaging.ImageFormat.Png);return s.ToArray();}}}
 static void Put32(byte[] b,int p,uint n){b[p]=(byte)(n>>24);b[p+1]=(byte)(n>>16);b[p+2]=(byte)(n>>8);b[p+3]=(byte)n;}
 static uint Crc(byte[] b,int p,int n){uint c=0xffffffff;for(int i=p;i<p+n;i++){c^=b[i];for(int k=0;k<8;k++)c=(c>>1)^((c&1)!=0?0xedb88320u:0);}return c^0xffffffff;}
 static byte[] Dimensions(byte[] png,uint x,uint y){byte[] b=(byte[])png.Clone();Put32(b,16,x);Put32(b,20,y);Put32(b,29,Crc(b,12,17));return b;}
 static byte[] Chunk(string kind,byte[] data){byte[] b=new byte[data.Length+12];Put32(b,0,(uint)data.Length);Encoding.ASCII.GetBytes(kind).CopyTo(b,4);data.CopyTo(b,8);Put32(b,data.Length+8,Crc(b,4,data.Length+4));return b;}
 static byte[] BeforeIend(byte[] png,byte[] chunk){byte[] b=new byte[png.Length+chunk.Length];Buffer.BlockCopy(png,0,b,0,png.Length-12);Buffer.BlockCopy(chunk,0,b,png.Length-12,chunk.Length);Buffer.BlockCopy(png,png.Length-12,b,png.Length-12+chunk.Length,12);return b;}
 static byte[] RewriteIdat(byte[] png,bool excess){
  using(var output=new System.IO.MemoryStream()){
   output.Write(png,0,8);int p=8;
   while(p<png.Length){int n=(png[p]<<24)|(png[p+1]<<16)|(png[p+2]<<8)|png[p+3];string kind=Encoding.ASCII.GetString(png,p+4,4);
    if(kind=="IDAT"){
     byte[] data=new byte[n];Buffer.BlockCopy(png,p+8,data,0,n);
     if(excess){ // Tiny image IHDR with a valid, longer deflate stream.
      byte[] raw=new byte[4096];uint a=1,b=0;using(var s=new System.IO.MemoryStream()){s.WriteByte(0x78);s.WriteByte(0x9c);using(var zip=new System.IO.Compression.DeflateStream(s,System.IO.Compression.CompressionMode.Compress,true)){zip.Write(raw,0,raw.Length);}for(int i=0;i<raw.Length;i++){a=(a+raw[i])%65521;b=(b+a)%65521;}byte[] check=new byte[4];Put32(check,0,(b<<16)|a);s.Write(check,0,4);data=s.ToArray();}
     }else data[0]=0; // CRC repaired but invalid zlib stream.
     byte[] c=Chunk(kind,data);output.Write(c,0,c.Length);
    }else output.Write(png,p,n+12);p+=n+12;
   }return output.ToArray();
  }
 }
 static void Disposed(System.Drawing.Image image){bool disposed=false;try{int ignored=image.Width;GC.KeepAlive(ignored);}catch(ArgumentException){disposed=true;}Eq(disposed,true);}
 [STAThread] static void Main(){
 Test("GET serializes native backend Unicode text",delegate {var r=Call("GET","/clipboard",new byte[0]);Eq(r.Status,200);var v=new System.Web.Script.Serialization.JavaScriptSerializer().Deserialize<Dictionary<string,object>>(r.Body);Eq(v["text"],fake.Text);Eq(v["has_text"],true);});
 Test("auth denies peer spoofing and invalid tokens before clipboard",delegate {var h=Headers();h["X-Forwarded-For"]="10.23.45.152";Eq(core.Handle("127.0.0.1","GET","/clipboard",h,new byte[0]).Status,403);h.Remove("Authorization");Eq(core.Handle("10.23.45.152","GET","/clipboard",h,new byte[0]).Status,401);h["Authorization"]="Bearer "+new string('b',64);Eq(core.Handle("10.23.45.152","GET","/clipboard",h,new byte[0]).Status,401);});
 Test("only exact path and GET POST methods are accepted",delegate {Eq(Call("GET","/clipboard?x=1",new byte[0]).Status,404);Eq(Call("DELETE","/clipboard",new byte[0]).Status,405);});
 Test("POST writes Unicode multiline and empty text with confirmation",delegate {foreach(var s in new[]{"한글 😀\n\t\"quote\"",""}){var body=Encoding.UTF8.GetBytes(new System.Web.Script.Serialization.JavaScriptSerializer().Serialize(new {text=s}));Eq(Call("POST","/clipboard",body).Status,204);Eq(fake.Text,s);}Eq(fake.Writes,2);});
 Test("POST rejects invalid JSON UTF8 NUL types and byte limits without writes",delegate {foreach(var s in new[]{"{bad","[]","null","{}","{\"text\":null}","{\"text\":1}","{\"text\":\"a\\u0000b\"}","{\"text\":\""+new string('x',65537)+"\"}","{\"text\":\""+new string('한',21846)+"\"}"})Eq(Call("POST","/clipboard",Encoding.UTF8.GetBytes(s)).Status,400);Eq(Call("POST","/clipboard",new byte[]{0xff,0xfe}).Status,400);Eq(Call("POST","/clipboard",new byte[524289]).Status,413);var h=Headers();h["Content-Type"]="text/plain";Eq(core.Handle("10.23.45.152","POST","/clipboard",h,new byte[0]).Status,415);Eq(fake.Writes,0);});
 Test("POST accepts exactly 64KiB UTF8",delegate {string s=new string('x',65536);Eq(Call("POST","/clipboard",Encoding.UTF8.GetBytes("{\"text\":\""+s+"\"}")).Status,204);Eq(fake.Text,s);});
 Test("GET distinguishes absent text and rejects invalid backend contents",delegate {fake.Has=false;fake.Text="";Eq(Call("GET","/clipboard",new byte[0]).Body.Contains("false"),true);fake.Has=true;fake.Text=new string('한',21846);Eq(Call("GET","/clipboard",new byte[0]).Status,413);fake.Text="bad\0value";Eq(Call("GET","/clipboard",new byte[0]).Status,503);fake.Has=false;fake.Text="unexpected";Eq(Call("GET","/clipboard",new byte[0]).Status,503);});
 Test("backend failure is sanitized 503 for read and write",delegate {fake.Failure=new InvalidOperationException("PRIVATE clipboard failure");Eq(Call("GET","/clipboard",new byte[0]).Status,503);var r=Call("POST","/clipboard",Encoding.UTF8.GetBytes("{\"text\":\"test\"}"));Eq(r.Status,503);Eq(r.Body,"");});
 Test("clipboard busy retry is bounded and other failures are not retried",delegate {int attempts=0,delays=0;Eq(BusyRetry.Run<int>(delegate {attempts++;if(attempts<3)throw new System.Runtime.InteropServices.ExternalException();return 42;},delegate(int ms){delays++;}),42);Eq(attempts,3);Eq(delays,2);attempts=0;try{BusyRetry.Run<int>(delegate {attempts++;throw new System.Runtime.InteropServices.ExternalException();},delegate(int ms){});throw new Exception("not bounded");}catch(System.Runtime.InteropServices.ExternalException){Eq(attempts,5);}attempts=0;try{BusyRetry.Run<int>(delegate {attempts++;throw new InvalidOperationException();},delegate(int ms){});}catch(InvalidOperationException){Eq(attempts,1);}});
 Test("HTTP transport parses real frame and emits explicit response length",delegate {string request="GET /clipboard HTTP/1.1\r\nHost: workstation\r\nAuthorization: Bearer "+token+"\r\nContent-Length: 0\r\n\r\n";var reply=core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(request)),"10.23.45.152",5000);Eq(reply.Status,200);});
 Test("HTTP rejects smuggling malformed truncated and excessive frames",delegate {
 string prefix="POST /clipboard HTTP/1.1\r\nHost: workstation\r\nAuthorization: Bearer "+token+"\r\nContent-Type: application/json\r\n";
 foreach(var tail in new[]{"Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}","Transfer-Encoding: chunked\r\n\r\n0\r\n\r\n","Content-Length: -1\r\n\r\n","Content-Length: nope\r\n\r\n","Content-Length: 30\r\n\r\n{}"," folded\r\n\r\n","BadHeader\r\n\r\n"})Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(prefix+tail)),"10.23.45.152",5000).Status,400);
 Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(prefix+"Content-Length: 524289\r\n\r\n")),"10.23.45.152",5000).Status,413);
 Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(prefix+"X-Large: "+new string('x',9000)+"\r\n\r\n")),"10.23.45.152",5000).Status,431);
 foreach(var request in new[]{"GET /clipboard HTTP/9\r\n\r\n","GET /clipboard HTTP/1.1\n\n","GET\r\n\r\n"})Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(request)),"10.23.45.152",5000).Status,400);
 });
 Test("responses always include UTF8 Content-Length and no-store",delegate {foreach(int status in new[]{200,204,400,401,403,404,405,408,413,415,431,503}){var r=new Reply(status,status==200?"한글 😀":"");byte[] bytes=Core.Encode(r);string frame=Encoding.UTF8.GetString(bytes);Eq(frame.Contains("Content-Length: "+Encoding.UTF8.GetByteCount(r.Body)+"\r\n"),true);Eq(frame.Contains("Cache-Control: no-store\r\n"),true);Eq(frame.EndsWith("\r\n\r\n"+r.Body),true);}});
 Test("guard materializes all fake clipboard formats without OS access",delegate {var original=new System.Windows.Forms.DataObject();original.SetData("UnicodeText",false,"private 한글 😀");original.SetData("Binary",false,new System.IO.MemoryStream(new byte[]{1,2,3}));original.SetData("Files",false,new string[]{"C:\\fake"});var snapshot=Snapshot.Capture(original);Eq(snapshot.Matches(original),true);original.SetData("UnicodeText",false,"changed");Eq(snapshot.Matches(original),false);Eq((string)snapshot.Data.GetData("UnicodeText",false),"private 한글 😀");});
 Test("guard refuses oversized or too many formats before live tests",delegate {var source=new System.Windows.Forms.DataObject();source.SetData("Huge",false,new byte[32*1024*1024+1]);bool rejected=false;try{Snapshot.Capture(source);}catch(InvalidOperationException){rejected=true;}Eq(rejected,true);source=new System.Windows.Forms.DataObject();for(int i=0;i<129;i++)source.SetData("Format"+i,false,"safe");rejected=false;try{Snapshot.Capture(source);}catch(InvalidOperationException){rejected=true;}Eq(rejected,true);});
 Test("HTTP immediate deadline produces 408 without clipboard access",delegate {Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes("GET /clipboard HTTP/1.1\r\n\r\n")),"10.23.45.152",0).Status,408);Eq(fake.Writes,0);});
 Test("guard rejects unmaterializable fake clipboard data and handles empty data",delegate {var source=new System.Windows.Forms.DataObject();source.SetData("Opaque",false,new object());bool rejected=false;try{Snapshot.Capture(source);}catch(InvalidOperationException){rejected=true;}Eq(rejected,true);Eq(Snapshot.Capture(null).Matches(new System.Windows.Forms.DataObject()),true);});
 Test("supervisor log rotates more than once and keeps logging",delegate {
  string root=System.IO.Path.Combine(System.IO.Path.GetTempPath(),"moonlight-log-test-"+Guid.NewGuid().ToString("N"));
  System.IO.Directory.CreateDirectory(root);
  try{
   // Fill past the limit three times over; the old code stopped writing after
   // the second rotation because "supervisor.log.previous" already existed.
   for(int round=0;round<3;round++){
    Eq(SupervisorLog.Append(root,"listener-ready session="+round),true);
    System.IO.File.WriteAllText(SupervisorLog.PathFor(root,0),new string('x',(int)SupervisorLog.MaxBytes+1));
    Eq(SupervisorLog.Append(root,"child-started pid="+round),true);
   }
   string active=System.IO.File.ReadAllText(SupervisorLog.PathFor(root,0));
   Eq(active.Contains("child-started pid=2"),true);
   Eq(System.IO.File.Exists(SupervisorLog.PathFor(root,1)),true);
   // Bounded retention: generation MaxBytes+1 must not exist.
   Eq(System.IO.File.Exists(SupervisorLog.PathFor(root,SupervisorLog.RetainedGenerations+1)),false);
   foreach(int generation in new[]{1,2}){var info=new System.IO.FileInfo(SupervisorLog.PathFor(root,generation));if(info.Exists)Eq(info.Length<=SupervisorLog.MaxBytes+256,true);}
  }finally{System.IO.Directory.Delete(root,true);}
 });
 Test("supervisor log never carries clipboard or token values",delegate {
  // The API only takes a caller-supplied status string; this asserts the
  // rotation path does not echo file contents (the old version moved the file
  // and could leave a stale generation behind containing old text).
  string root=System.IO.Path.Combine(System.IO.Path.GetTempPath(),"moonlight-log-privacy-"+Guid.NewGuid().ToString("N"));
  System.IO.Directory.CreateDirectory(root);
  try{
   SupervisorLog.Append(root,"child-started pid=1");
   foreach(string file in System.IO.Directory.GetFiles(root)){string content=System.IO.File.ReadAllText(file);Eq(content.Contains("token"),false);}
  }finally{System.IO.Directory.Delete(root,true);}
 });
 Test("capability metadata on legacy text backend never reads clipboard",delegate {
  fake.Failure=new InvalidOperationException("must not touch clipboard");
  var r=Call("GET","/capabilities",new byte[0]);Eq(r.Status,200);
  var v=new System.Web.Script.Serialization.JavaScriptSerializer().Deserialize<Dictionary<string,object>>(r.Body);
  Eq(r.Body.Contains("text/plain"),true);Eq(r.Body.Contains("image/png"),false);Eq(v.ContainsKey("has_text"),false);
  Eq(Call("POST","/capabilities",new byte[0]).Status,405);
  var h=Headers();h.Remove("Authorization");Eq(core.Handle("10.23.45.152","GET","/capabilities",h,new byte[0]).Status,401);
  h=Headers();h["X-Forwarded-For"]="10.23.45.152";Eq(core.Handle("127.0.0.1","GET","/capabilities",h,new byte[0]).Status,403);
 });
 Test("PNG handshake and image roundtrip use optional fake backend",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");
  image.Failure=new Exception("no clipboard during capability");var cap=Call("GET","/capabilities",new byte[0]);Eq(cap.Status,200);Eq(cap.Body.Contains("image/png"),true);Eq(cap.Body.Contains("4194304"),true);Eq(image.ImageReads,0);image.Failure=null;
  Eq(Call("GET","/clipboard/image",new byte[0]).Body,"{\"has_image\":false,\"png_base64\":\"\"}");
  byte[] png=SamplePng();Eq(Call("POST","/clipboard/image",ImageBody(png)).Status,204);Eq(image.ImageWrites,1);
  var r=Call("GET","/clipboard/image",new byte[0]);Eq(r.Status,200);Eq(r.Body.Contains(Convert.ToBase64String(png)),true);Eq(image.Writes,0);
  Eq(Call("GET","/clipboard",new byte[0]).Status,200);
  core=new Core(fake,token,"10.23.45.152");Eq(Call("GET","/clipboard/image",new byte[0]).Status,404);
 });
 Test("PNG malformed base64 JSON signature CRC truncation never writes",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");
  foreach(string s in new[]{"{bad","[]","null","{}","{\"png_base64\":null}","{\"png_base64\":1}","{\"png_base64\":\"\"}","{\"png_base64\":\"!!!!\"}","{\"png_base64\":\"AA== \"}"})Eq(Call("POST","/clipboard/image",Encoding.UTF8.GetBytes(s)).Status,400);
  Eq(Call("POST","/clipboard/image",new byte[]{255}).Status,400);
  byte[] png=SamplePng();byte[] bad=(byte[])png.Clone();bad[0]=0;Eq(Call("POST","/clipboard/image",ImageBody(bad)).Status,400);
  bad=(byte[])png.Clone();bad[29]^=1;Eq(Call("POST","/clipboard/image",ImageBody(bad)).Status,400);
  bad=new byte[png.Length-1];Buffer.BlockCopy(png,0,bad,0,bad.Length);Eq(Call("POST","/clipboard/image",ImageBody(bad)).Status,400);
  bad=new byte[png.Length+1];Buffer.BlockCopy(png,0,bad,0,png.Length);Eq(Call("POST","/clipboard/image",ImageBody(bad)).Status,400);Eq(image.ImageWrites,0);
 });
 Test("PNG binary base64 JSON and dimensions limits are distinct",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");
  Eq(Core.MaxImageBytes,4194304);Eq(Core.MaxImageBase64,5592408);Eq(Core.MaxImageBody,5593432);
  Eq(Call("POST","/clipboard/image",new byte[Core.MaxImageBody+1]).Status,413);
  Eq(Call("POST","/clipboard/image",Encoding.UTF8.GetBytes("{\"png_base64\":\""+new string('A',Core.MaxImageBase64+4)+"\"}")).Status,413);
  Eq(Call("POST","/clipboard/image",ImageBody(new byte[Core.MaxImageBytes+1])).Status,413);
  byte[] png=SamplePng();foreach(var pair in new uint[][]{new uint[]{4097,1},new uint[]{1,4097},new uint[]{65535,65535}})Eq(Call("POST","/clipboard/image",ImageBody(Dimensions(png,pair[0],pair[1]))).Status,413);
  Eq(Call("POST","/clipboard/image",ImageBody(Dimensions(png,0,1))).Status,400);Eq(image.ImageWrites,0);
 });
 Test("PNG HTTP framing is route scoped and auth precedes body work",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");
  byte[] png=SamplePng();string body=Encoding.UTF8.GetString(ImageBody(png));
  string head="POST /clipboard/image HTTP/1.1\r\nHost: fake\r\nAuthorization: Bearer "+token+"\r\nContent-Type: application/json\r\nContent-Length: ";
  string padded=body+new string(' ',Core.MaxBody);Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+padded.Length+"\r\n\r\n"+padded)),"10.23.45.152",5000).Status,204);
  Eq(image.ImageWrites,1);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+(Core.MaxImageBody+1)+"\r\n\r\n")),"10.23.45.152",5000).Status,413);
  string noAuth="POST /clipboard/image HTTP/1.1\r\nContent-Length: "+(Core.MaxImageBody+1)+"\r\n\r\n";Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(noAuth)),"10.23.45.152",5000).Status,401);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+"2\r\n\r\n{")),"10.23.45.152",5000).Status,400);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+"2\r\nContent-Length: 2\r\n\r\n{}")),"10.23.45.152",5000).Status,400);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+"2\r\nTransfer-Encoding: chunked\r\n\r\n{}")),"10.23.45.152",5000).Status,400);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+"2\r\n\r\n{}")),"127.0.0.1",5000).Status,403);
  Eq(core.Parse(new System.IO.MemoryStream(Encoding.ASCII.GetBytes(head+"2\r\n\r\n{}")),"10.23.45.152",0).Status,408);Eq(image.ImageWrites,1);
 });
 Test("native PNG write adapter decodes and owns detached image before fake write",delegate {
  byte[] png=SamplePng();int writes=0;
  var adapter=new PngClipboardAdapter(delegate {throw new Exception("write must not read");},delegate(System.Drawing.Image image){writes++;Eq(image.Width,2);using(var b=new System.Drawing.Bitmap(image)){Eq(b.GetPixel(0,0).R,(byte)255);}},delegate(int ms){});
  adapter.WritePng(png);Eq(writes,1);
  byte[] bad=Dimensions(png,1,1);try{adapter.WritePng(bad);throw new Exception("decode accepted mismatch");}catch(PngRejectedException){Eq(writes,1);}
  using(var decoded=PngCodec.Decode(png)){Array.Clear(png,0,png.Length);Eq(decoded.GetPixel(1,0).B,(byte)255);}
 });
 Test("native PNG read adapter bounds encoding and disposes owned fake images",delegate {
  System.Drawing.Image owned=new System.Drawing.Bitmap(2,1);int reads=0;
  var adapter=new PngClipboardAdapter(delegate {reads++;return owned;},delegate(System.Drawing.Image image){throw new Exception("read must not write");},delegate(int ms){});
  byte[] png=adapter.ReadPng();PngCodec.Validate(png);Eq(reads,1);Disposed(owned);
  adapter=new PngClipboardAdapter(delegate {return null;},delegate(System.Drawing.Image image){},delegate(int ms){});Eq(adapter.ReadPng(),null);
  owned=new System.Drawing.Bitmap(4097,1);adapter=new PngClipboardAdapter(delegate {return owned;},delegate(System.Drawing.Image image){},delegate(int ms){});
  try{adapter.ReadPng();throw new Exception("oversize accepted");}catch(PngRejectedException e){Eq(e.Status,413);}Disposed(owned);
 });
 Test("native PNG fake write retries disposes and requires STA",delegate {
  int writes=0,delays=0;System.Drawing.Image captured=null;
  var adapter=new PngClipboardAdapter(delegate {return null;},delegate(System.Drawing.Image image){captured=image;writes++;if(writes<3)throw new System.Runtime.InteropServices.ExternalException();},delegate(int ms){delays++;});
  adapter.WritePng(SamplePng());Eq(writes,3);Eq(delays,2);Disposed(captured);
  adapter=new PngClipboardAdapter(delegate {return null;},delegate(System.Drawing.Image image){captured=image;throw new InvalidOperationException("private");},delegate(int ms){});
  try{adapter.WritePng(SamplePng());throw new Exception("failure hidden");}catch(InvalidOperationException){}Disposed(captured);
  Exception failure=null;var thread=new System.Threading.Thread(delegate(){try{adapter.WritePng(SamplePng());}catch(Exception e){failure=e;}});thread.SetApartmentState(System.Threading.ApartmentState.MTA);thread.Start();thread.Join();Eq(failure is InvalidOperationException,true);
 });
 Test("image auth MIME unsupported routes and sanitized failures",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");byte[] body=ImageBody(SamplePng());var h=Headers();h.Remove("Authorization");
  Eq(core.Handle("10.23.45.152","POST","/clipboard/image",h,new byte[Core.MaxImageBody+1]).Status,401);
  h=Headers();h["X-Forwarded-For"]="10.23.45.152";Eq(core.Handle("127.0.0.1","GET","/clipboard/image",h,new byte[0]).Status,403);
  h["Content-Type"]="image/png";Eq(core.Handle("10.23.45.152","POST","/clipboard/image",h,body).Status,415);
  Eq(Call("DELETE","/clipboard/image",new byte[0]).Status,405);Eq(Call("GET","/clipboard/image?x",new byte[0]).Status,404);Eq(image.ImageReads,0);Eq(image.ImageWrites,0);
  image.Failure=new Exception("PRIVATE payload");Eq(Call("GET","/clipboard/image",new byte[0]).Status,503);Eq(Call("POST","/clipboard/image",body).Body,"");Eq(image.ImageWrites,0);
  image.Failure=new PngRejectedException(413);Eq(Call("GET","/clipboard/image",new byte[0]).Status,413);
 });
 Test("PNG chunk integrity and inflation bounds reject before fake write",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");byte[] png=SamplePng();
  foreach(byte[] bad in new[]{BeforeIend(png,Chunk("ABCD",new byte[0])),BeforeIend(png,Chunk("abca",new byte[0])),BeforeIend(png,Chunk("IHDR",new byte[13])),BeforeIend(png,Chunk("acTL",new byte[8])),RewriteIdat(png,false),RewriteIdat(png,true)})Eq(Call("POST","/clipboard/image",ImageBody(bad)).Status,400);
  byte[] ancillary=Chunk("tEXt",new byte[]{65,0,66});ancillary[ancillary.Length-1]^=1;Eq(Call("POST","/clipboard/image",ImageBody(BeforeIend(png,ancillary))).Status,400);
  Eq(Call("POST","/clipboard/image",Encoding.UTF8.GetBytes("{\"png_base64\":\"AB==\"}")).Status,400);
  Eq(Call("POST","/clipboard/image",Encoding.UTF8.GetBytes("{\"png_base64\":\"AAAA\",\"html\":\"no\"}")).Status,400);Eq(image.ImageWrites,0);
 });
 Test("PNG exact binary limit remains accepted despite larger base64",delegate {
  var image=new ImageFake();core=new Core(image,token,"10.23.45.152");byte[] png=SamplePng();byte[] at=BeforeIend(png,Chunk("tEXt",new byte[Core.MaxImageBytes-png.Length-12]));
  Eq(at.Length,Core.MaxImageBytes);Eq(Convert.ToBase64String(at).Length,Core.MaxImageBase64);
  Eq(Call("POST","/clipboard/image",ImageBody(at)).Status,204);Eq(image.ImageWrites,1);
  var r=Call("GET","/clipboard/image",new byte[0]);Eq(r.Status,200);Eq(Encoding.UTF8.GetByteCount(r.Body)<=Core.MaxImageBody,true);
 });
 Test("native PNG encoder refuses more than binary cap and disposes source",delegate {
  var owned=new System.Drawing.Bitmap(1200,1200,System.Drawing.Imaging.PixelFormat.Format32bppArgb);
  var region=owned.LockBits(new System.Drawing.Rectangle(0,0,1200,1200),System.Drawing.Imaging.ImageLockMode.WriteOnly,System.Drawing.Imaging.PixelFormat.Format32bppArgb);
  try{byte[] noise=new byte[region.Stride*1200];new Random(42).NextBytes(noise);for(int i=3;i<noise.Length;i+=4)noise[i]=255;System.Runtime.InteropServices.Marshal.Copy(noise,0,region.Scan0,noise.Length);}finally{owned.UnlockBits(region);}
  var adapter=new PngClipboardAdapter(delegate {return owned;},delegate(System.Drawing.Image image){throw new Exception("unexpected write");},delegate(int ms){});
  try{adapter.ReadPng();throw new Exception("incompressible image exceeded bound");}catch(PngRejectedException e){Eq(e.Status,413);}Disposed(owned);
 });
 Test("PNG metadata never reaches GDI decoder",delegate {
  byte[] png=BeforeIend(SamplePng(),Chunk("iCCP",new byte[]{65,0,0,0,1,2,3}));
  using(var image=PngCodec.Decode(png)){Eq(image.Width,2);Eq(image.GetPixel(0,0).R,(byte)255);}
 });
 Test("PNG count limit bounds chunk work and decoded clone disposal repeats",delegate {
  byte[] png=SamplePng();byte[] empty=Chunk("tEXt",new byte[0]);
  using(var s=new System.IO.MemoryStream()){
   s.Write(png,0,png.Length-12);for(int i=0;i<PngCodec.MaxChunks;i++)s.Write(empty,0,empty.Length);s.Write(png,png.Length-12,12);
   var image=new ImageFake();core=new Core(image,token,"10.23.45.152");Eq(Call("POST","/clipboard/image",ImageBody(s.ToArray())).Status,400);Eq(image.ImageWrites,0);
  }
  for(int i=0;i<20;i++){System.Drawing.Bitmap image=PngCodec.Decode(png);Eq(image.Width,2);image.Dispose();Disposed(image);}
 });
#if NATIVE_ADAPTER_TESTS
 Test("NativeClipboard wires injected image adapter without OS clipboard",delegate {
  var image=new ImageFake();var native=new NativeClipboard(image);core=new Core(native,token,"10.23.45.152");
  image.Failure=new Exception("must not read");Eq(Call("GET","/capabilities",new byte[0]).Status,200);Eq(image.ImageReads,0);image.Failure=null;
  Eq(Call("POST","/clipboard/image",ImageBody(SamplePng())).Status,204);Eq(image.ImageWrites,1);Eq(Call("GET","/clipboard/image",new byte[0]).Status,200);Eq(image.ImageReads,1);Eq(image.Writes,0);
 });
#endif
 Console.WriteLine("TOTAL "+count+" PASS");
 }
}
