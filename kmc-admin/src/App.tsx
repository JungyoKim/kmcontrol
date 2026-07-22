import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Toaster } from "@/components/ui/sonner";
import { Dashboard } from "@/Dashboard";

export default function App() {
  const [loggedIn, setLoggedIn] = useState(false);

  return (
    <>
      <Toaster richColors position="top-right" />
      {loggedIn ? <Dashboard /> : <LoginForm onSuccess={() => setLoggedIn(true)} />}
    </>
  );
}

function LoginForm({ onSuccess }: { onSuccess: () => void }) {
  const [hubUrl, setHubUrl] = useState("http://127.0.0.1:8080");
  const [username, setUsername] = useState("admin");
  const [password, setPassword] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  async function submit(e: React.FormEvent) {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await invoke("login", { hubUrl, username, password });
      onSuccess();
    } catch (err) {
      setError(String(err));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-background p-4">
      <Card className="w-full max-w-sm">
        <CardHeader>
          <CardTitle>KMC 관리 콘솔 로그인</CardTitle>
        </CardHeader>
        <CardContent>
          <form onSubmit={submit} className="space-y-4">
            <div className="space-y-2">
              <Label htmlFor="hub">Hub URL</Label>
              <Input id="hub" value={hubUrl} onChange={(e) => setHubUrl(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="user">사용자명</Label>
              <Input id="user" value={username} onChange={(e) => setUsername(e.target.value)} />
            </div>
            <div className="space-y-2">
              <Label htmlFor="pw">비밀번호</Label>
              <Input
                id="pw"
                type="password"
                value={password}
                onChange={(e) => setPassword(e.target.value)}
              />
            </div>
            {error && <p className="text-sm text-destructive">{error}</p>}
            <Button type="submit" className="w-full" disabled={busy}>
              {busy ? "로그인 중..." : "로그인"}
            </Button>
          </form>
        </CardContent>
      </Card>
    </div>
  );
}
