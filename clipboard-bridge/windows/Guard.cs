using System;
using System.IO;
using System.Threading;
using System.Diagnostics;
using System.Windows.Forms;
using MoonlightClipboard;
class Guard {
 static T Retry<T>(Func<T> action){return BusyRetry.Run<T>(action,Thread.Sleep);}
 static string Root=Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),"MoonlightClipboard-build","guard");
 static void State(string text){File.WriteAllText(Path.Combine(Root,"status"),text);}
 [STAThread] static int Main(string[] args){
  // The verification tool must present the same nonce it will look for in
  // `ready`; a default random nonce keeps two concurrent runs from crossing.
  string run=Environment.GetEnvironmentVariable("MOONLIGHT_CLIPBOARD_GUARD_RUN");
  if(string.IsNullOrEmpty(run))run=Guid.NewGuid().ToString("N");
  // The lease bounds how long a verification may keep writing. After this the
  // guard restores the clipboard and the verification's per-write re-check
  // starts failing instead of overwriting the restored state.
  int leaseSeconds=180;
  string configured=Environment.GetEnvironmentVariable("MOONLIGHT_CLIPBOARD_GUARD_LEASE_SECONDS");
  if(!string.IsNullOrEmpty(configured)){int parsed;if(int.TryParse(configured,out parsed)&&parsed>0&&parsed<=1800)leaseSeconds=parsed;}
  Snapshot snapshot=null;bool ready=false;
  try {
   if(Process.GetCurrentProcess().SessionId==0)throw new InvalidOperationException();
   Directory.CreateDirectory(Root);
   snapshot=Retry<Snapshot>(delegate{return Snapshot.Capture(Clipboard.GetDataObject());});
   if(!snapshot.Matches(snapshot.Data))throw new InvalidOperationException();
   // A stale lease from an earlier run must not authorise this one.
   foreach(string stale in new[]{"ready","done","restored"}){string p=Path.Combine(Root,stale);if(File.Exists(p))File.Delete(p);}
   long deadline=DateTimeOffset.UtcNow.ToUnixTimeSeconds()+leaseSeconds;
   // No content or fingerprints leave memory. Ready means every format
   // materialized, and it names the run/session/absolute deadline it covers.
   State("ready session="+Process.GetCurrentProcess().SessionId+" run="+run);
   File.WriteAllText(Path.Combine(Root,"ready"),"ready session="+Process.GetCurrentProcess().SessionId+" run="+run+" deadline="+deadline);
   ready=true;
   var clock=Stopwatch.StartNew();while(clock.Elapsed.TotalSeconds<leaseSeconds&&!File.Exists(Path.Combine(Root,"done"))){Thread.Sleep(100);}
   return 0;
  }catch{State("capture-failed-no-test-authorized");return 1;}
  finally {
   if(ready&&snapshot!=null){
    bool restored=false;
    for(int attempt=0;attempt<30&&!restored;attempt++){
     try{Retry<int>(delegate {if(snapshot.Data.GetFormats(false).Length==0)Clipboard.Clear();else Clipboard.SetDataObject(snapshot.Data,true,0,0);return 0;});
      restored=Retry<bool>(delegate{return snapshot.Matches(Clipboard.GetDataObject());});
     }catch{}if(!restored)Thread.Sleep(100);
    }
    State(restored?"restored-all-formats-verified run="+run:"RESTORE-FAILED run="+run);
    File.WriteAllText(Path.Combine(Root,"restored"),restored?"true":"false");
   }
  }
 }
}
