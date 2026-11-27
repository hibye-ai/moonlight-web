using System;
using System.IO;
using System.Net;
using System.Net.Sockets;
using System.Diagnostics;
using System.Threading;
using System.Windows.Forms;
using System.Web.Script.Serialization;
using System.Collections.Generic;
namespace MoonlightClipboard {
public sealed class NativeClipboard : IClipboard, IImageClipboard {
 private readonly IImageClipboard images;
 public NativeClipboard():this(PngClipboardAdapter.CreateNative()){}
 // Injection is image-only; fake tests must never call the text OS methods.
 public NativeClipboard(IImageClipboard adapter){if(adapter==null)throw new ArgumentNullException("adapter");images=adapter;}
 public byte[] ReadPng(){return images.ReadPng();}
 public void WritePng(byte[] png){images.WritePng(png);}
 public Contents Read(){return BusyRetry.Run<Contents>(delegate {
  bool has=Clipboard.ContainsText(TextDataFormat.UnicodeText);
  return new Contents(has,has?Clipboard.GetText(TextDataFormat.UnicodeText):"");
 },Thread.Sleep);}
 public void Write(string text){BusyRetry.Run<int>(delegate {
  var data=new DataObject();data.SetData(DataFormats.UnicodeText,false,text);
  Clipboard.SetDataObject(data,true,0,0);return 0;
 },Thread.Sleep);}
}
public static class Program {
 static readonly string Root=AppDomain.CurrentDomain.BaseDirectory;
 static void Status(string value){SupervisorLog.Append(Root,value);}
 [STAThread] public static int Main(string[] args){
  try {
   bool supervise=args.Length==1&&args[0]=="--supervise";
   if(args.Length>0&&!supervise)return 2;
   if(Process.GetCurrentProcess().SessionId==0)return 3;
   bool created;using(var mutex=new Mutex(true,"Local\\MoonlightClipboard-"+(supervise?"supervisor":"bridge"),out created)){
    if(!created)return 0;
    if(supervise){
     while(true){
      try{var info=new ProcessStartInfo(Path.Combine(Root,"MoonlightClipboard.exe"));info.UseShellExecute=false;info.CreateNoWindow=true;info.WindowStyle=ProcessWindowStyle.Hidden;info.WorkingDirectory=Root;
       using(var child=Process.Start(info)){Status("child-started pid="+child.Id);child.WaitForExit();Status("child-exited code="+child.ExitCode);}
      }catch{Status("child-start-failed");}
      Thread.Sleep(2000);
     }
    }
    var config=new JavaScriptSerializer().Deserialize<Dictionary<string,object>>(File.ReadAllText(Path.Combine(Root,"config.json")));
    string token=(string)config["token"],bind=(string)config["bind"];
    int port=Convert.ToInt32(config["port"]);var peers=config["allowed_ips"] as System.Collections.ArrayList;
    if(token.Length!=64||!System.Text.RegularExpressions.Regex.IsMatch(token,"\\A[0-9a-f]{64}\\z")||peers==null||peers.Count!=1||port<1024||port>65535)return 4;
    IPAddress ip=IPAddress.Parse(bind);if(IPAddress.Any.Equals(ip)||IPAddress.Loopback.Equals(ip))return 4;
    string peer=(string)peers[0];IPAddress.Parse(peer);
    var core=new Core(new NativeClipboard(),token,peer);var listener=new TcpListener(ip,port);listener.Start(8);
    Status("listener-ready session="+Process.GetCurrentProcess().SessionId);
    while(true){using(var client=listener.AcceptTcpClient()){
      client.ReceiveTimeout=5000;client.SendTimeout=1000;client.NoDelay=true;
      try{using(var stream=client.GetStream()){var reply=core.Parse(stream,((IPEndPoint)client.Client.RemoteEndPoint).Address.ToString(),5000);byte[] bytes=Core.Encode(reply);stream.WriteTimeout=1000;stream.Write(bytes,0,bytes.Length);}}
      catch{} // No request, token, text, or exception data is logged.
    }}
   }
  }catch{Status("bridge-failed");return 1;}
 }
}
}
